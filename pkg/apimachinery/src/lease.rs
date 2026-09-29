//! Local observation of Kubernetes Lease records, independent of wall-clock offsets.
use serde_json::Value;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A term lasts this long past the start of its last successful renewal
/// attempt (upstream `renewDeadline`). It is shorter than the 15 s lease, and
/// a candidate measures that lease from when it *observes* the renewal, which
/// is after the attempt started: the old term ends before a takeover.
pub const RENEW_DEADLINE: Duration = Duration::from_secs(10);
/// Upper bound on one renewal request.
const RENEW_ATTEMPT: Duration = Duration::from_secs(5);

// tokio's clock, so tests can pause and advance it; in production it is the
// monotonic system clock.
pub type TermInstant = tokio::time::Instant;
enum Permission {
    Unrestricted,
    Follower,
    Leader(TermInstant),
}

/// Every mutation checks this monotonic deadline. A paused former leader
/// cannot begin fresh work merely because its cancellation task has not run.
#[derive(Clone)]
pub struct WriteGate(Arc<Mutex<Permission>>);
impl Default for WriteGate {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Permission::Unrestricted)))
    }
}
impl WriteGate {
    pub fn close(&self) {
        *self.0.lock().unwrap() = Permission::Follower;
    }
    /// Begin a term whose acquiring attempt started at `attempted`.
    pub fn start(&self, attempted: TermInstant) {
        *self.0.lock().unwrap() = Permission::Leader(attempted + RENEW_DEADLINE);
    }
    /// Extend a live term after a renewal attempt that started at `attempted`
    /// succeeded. An expired term is never revived.
    pub fn renew(&self, attempted: TermInstant) -> bool {
        let mut state = self.0.lock().unwrap();
        match *state {
            Permission::Leader(expires) if TermInstant::now() < expires => {
                *state = Permission::Leader(expires.max(attempted + RENEW_DEADLINE));
                true
            }
            _ => {
                *state = Permission::Follower;
                false
            }
        }
    }
    /// Time left in the current term; `None` unless leading.
    fn remaining(&self) -> Option<Duration> {
        match *self.0.lock().unwrap() {
            Permission::Leader(expires) => {
                Some(expires.saturating_duration_since(TermInstant::now()))
            }
            _ => None,
        }
    }
    pub fn budget(&self) -> anyhow::Result<Duration> {
        match *self.0.lock().unwrap() {
            Permission::Unrestricted => Ok(Duration::from_secs(30)),
            Permission::Leader(expires) => {
                let remaining = expires.saturating_duration_since(TermInstant::now());
                anyhow::ensure!(!remaining.is_zero(), "leadership renewal deadline expired");
                Ok(remaining.min(Duration::from_secs(5)))
            }
            Permission::Follower => anyhow::bail!("mutations require an active leadership term"),
        }
    }
}

/// Keep a leadership term renewed; returns (with the gate closed) once it is
/// over. Renewal runs on its own schedule, independent of the work the term
/// performs. A failed or hung attempt is retried every `retry` while the term
/// lasts, so one lost request does not end it; the term ends when no attempt
/// has succeeded for [`RENEW_DEADLINE`] from its start. Each attempt is bounded
/// by the time the term has left.
pub async fn hold<F, Fut>(gate: &WriteGate, retry: Duration, mut renew: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    loop {
        let Some(remaining) = gate.remaining() else {
            break;
        };
        tokio::time::sleep(retry.min(remaining)).await;
        let Some(remaining) = gate.remaining().filter(|r| !r.is_zero()) else {
            break;
        };
        let attempted = TermInstant::now();
        let renewed = tokio::time::timeout(remaining.min(RENEW_ATTEMPT), renew())
            .await
            .unwrap_or(false);
        if renewed {
            if !gate.renew(attempted) {
                break;
            }
        } else {
            tracing::warn!("leadership renewal failed; retrying while the term lasts");
        }
    }
    gate.close();
}

#[derive(Default)]
pub struct Observation {
    record: Option<Value>,
    observed_at: Option<Instant>,
}

impl Observation {
    /// A foreign holder must remain unchanged for its entire lease duration.
    /// Remote timestamps indicate change only; their absolute time is untrusted.
    pub fn expired(&mut self, spec: &Value, duration: Duration, now: Instant) -> bool {
        if self.record.as_ref() != Some(spec) {
            self.record = Some(spec.clone());
            self.observed_at = Some(now);
        }
        now.saturating_duration_since(self.observed_at.unwrap()) >= duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn expired_term_cannot_write_or_be_revived_by_late_renewal() {
        let gate = WriteGate::default();
        gate.close();
        assert!(gate.budget().is_err());
        gate.start(TermInstant::now());
        assert!(gate.budget().is_ok());
        *gate.0.lock().unwrap() =
            Permission::Leader(TermInstant::now() - Duration::from_secs(1));
        assert!(gate.budget().is_err());
        assert!(!gate.renew(TermInstant::now()));
        assert!(gate.budget().is_err());
    }

    fn leading() -> WriteGate {
        let gate = WriteGate::default();
        gate.start(TermInstant::now());
        gate
    }

    const RETRY: Duration = Duration::from_secs(2);

    #[tokio::test(start_paused = true)]
    async fn transient_renewal_failures_keep_the_term() {
        let gate = leading();
        let attempts = std::cell::Cell::new(0u32);
        let began = TermInstant::now();
        // Fail every other attempt: never 10 s without a success.
        hold(&gate, RETRY, || {
            let n = attempts.get() + 1;
            attempts.set(n);
            async move { n < 20 && n % 2 == 0 }
        })
        .await;
        assert!(attempts.get() >= 20, "ended after {} attempts", attempts.get());
        assert!(gate.budget().is_err(), "hold returns with the gate closed");
        assert!(began.elapsed() >= Duration::from_secs(40));
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_failure_ends_the_term_by_the_deadline() {
        let gate = leading();
        let began = TermInstant::now();
        hold(&gate, RETRY, || async { false }).await;
        assert!(gate.budget().is_err());
        assert!(began.elapsed() <= RENEW_DEADLINE, "{:?}", began.elapsed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_renewal_cannot_outlive_the_term() {
        let gate = leading();
        let began = TermInstant::now();
        hold(&gate, RETRY, || std::future::pending::<bool>()).await;
        assert!(gate.budget().is_err());
        assert!(began.elapsed() <= RENEW_DEADLINE, "{:?}", began.elapsed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_success_extends_from_its_start_not_its_end() {
        let gate = leading();
        let first = std::cell::Cell::new(true);
        let began = TermInstant::now();
        // First renewal starts at 2 s and takes 4 s; then all fail.
        hold(&gate, RETRY, || {
            let slow = first.replace(false);
            async move {
                if slow {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                }
                slow
            }
        })
        .await;
        // Term ends at 2 s + RENEW_DEADLINE, not at 6 s + RENEW_DEADLINE.
        assert!(began.elapsed() <= RETRY + RENEW_DEADLINE, "{:?}", began.elapsed());
        assert!(began.elapsed() >= RENEW_DEADLINE);
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_gate_ends_hold_without_renewing() {
        let gate = leading();
        gate.close();
        let attempts = std::cell::Cell::new(0u32);
        hold(&gate, RETRY, || {
            attempts.set(attempts.get() + 1);
            async { true }
        })
        .await;
        assert_eq!(attempts.get(), 0);
    }

    #[test]
    fn remote_wall_clock_does_not_expire_a_newly_observed_holder() {
        for timestamp in ["1970-01-01T00:00:00Z", "2099-01-01T00:00:00Z", "invalid"] {
            let mut observation = Observation::default();
            let spec = json!({"holderIdentity": "other", "renewTime": timestamp});
            let now = Instant::now();
            let duration = Duration::from_secs(15);
            assert!(!observation.expired(&spec, duration, now));
            assert!(!observation.expired(&spec, duration, now + Duration::from_secs(14)));
            assert!(observation.expired(&spec, duration, now + duration));
        }
    }

    #[test]
    fn observed_renewal_restarts_local_expiration() {
        let mut observation = Observation::default();
        let now = Instant::now();
        let duration = Duration::from_secs(15);
        let first = json!({"holderIdentity": "other", "renewTime": "a"});
        let renewed = json!({"holderIdentity": "other", "renewTime": "b"});
        assert!(!observation.expired(&first, duration, now));
        assert!(!observation.expired(&renewed, duration, now + Duration::from_secs(14)));
        assert!(!observation.expired(&renewed, duration, now + Duration::from_secs(15)));
        assert!(observation.expired(&renewed, duration, now + Duration::from_secs(29)));
    }
}
