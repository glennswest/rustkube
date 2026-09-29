//! Local observation of Kubernetes Lease records, independent of wall-clock offsets.
use serde_json::Value;
use std::time::{Duration, Instant};

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
