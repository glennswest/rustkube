//! Local observation of Kubernetes Lease records, independent of wall-clock offsets.
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const RENEW_DEADLINE: Duration = Duration::from_secs(10);
enum Permission {
    Unrestricted,
    Follower,
    Leader(Instant),
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
    pub fn start(&self) {
        *self.0.lock().unwrap() = Permission::Leader(Instant::now() + RENEW_DEADLINE);
    }
    pub fn renew(&self) -> bool {
        let mut state = self.0.lock().unwrap();
        match *state {
            Permission::Leader(expires) if Instant::now() < expires => {
                *state = Permission::Leader(Instant::now() + RENEW_DEADLINE);
                true
            }
            _ => {
                *state = Permission::Follower;
                false
            }
        }
    }
    pub fn budget(&self) -> anyhow::Result<Duration> {
        match *self.0.lock().unwrap() {
            Permission::Unrestricted => Ok(Duration::from_secs(30)),
            Permission::Leader(expires) => {
                let remaining = expires.saturating_duration_since(Instant::now());
                anyhow::ensure!(!remaining.is_zero(), "leadership renewal deadline expired");
                Ok(remaining.min(Duration::from_secs(5)))
            }
            Permission::Follower => anyhow::bail!("mutations require an active leadership term"),
        }
    }
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
        gate.start();
        assert!(gate.budget().is_ok());
        *gate.0.lock().unwrap() = Permission::Leader(Instant::now() - Duration::from_secs(1));
        assert!(gate.budget().is_err());
        assert!(!gate.renew());
        assert!(gate.budget().is_err());
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
