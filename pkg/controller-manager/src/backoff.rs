//! Shared recreation backoff for workload controllers.
//!
//! When a controller-owned pod fails and is replaced, an immediately-failing
//! image would otherwise churn create/delete every reconcile. This tracks a
//! per-key exponential backoff (CrashLoopBackOff-style) so replacements are
//! spaced out: `BASE * 2^(failures-1)`, capped at `MAX`. Keys are chosen by the
//! caller — a ReplicaSet uid, or a DaemonSet-uid/node pair.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const BASE_SECS: u64 = 10;
const MAX_SECS: u64 = 300;

struct Entry {
    failures: u32,
    next: Instant,
}

/// Per-key create backoff, shared across reconciles on a controller.
#[derive(Default)]
pub struct CreateBackoff {
    inner: Mutex<HashMap<String, Entry>>,
}

impl CreateBackoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an observed failure for `key`, widening its backoff window.
    pub fn record_failure(&self, key: &str, now: Instant) {
        let mut m = self.inner.lock().unwrap();
        let e = m.entry(key.to_string()).or_insert(Entry {
            failures: 0,
            next: now,
        });
        e.failures = e.failures.saturating_add(1);
        e.next = now + delay(e.failures);
    }

    /// Clear any backoff for `key` (the workload is stable/healthy again).
    pub fn clear(&self, key: &str) {
        self.inner.lock().unwrap().remove(key);
    }

    /// Whether a create is allowed for `key` at `now` (no active backoff window).
    pub fn allowed(&self, key: &str, now: Instant) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(key)
            .map(|e| now >= e.next)
            .unwrap_or(true)
    }
}

/// Exponential delay: `BASE * 2^(failures-1)`, capped at `MAX`. `failures` is
/// 1-based (first failure → BASE).
pub fn delay(failures: u32) -> Duration {
    let shift = failures.clamp(1, 6) - 1; // cap the shift so BASE<<shift can't overflow
    Duration::from_secs((BASE_SECS << shift).min(MAX_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_grows_then_caps() {
        assert_eq!(delay(1), Duration::from_secs(10));
        assert_eq!(delay(2), Duration::from_secs(20));
        assert_eq!(delay(3), Duration::from_secs(40));
        assert_eq!(delay(4), Duration::from_secs(80));
        assert_eq!(delay(5), Duration::from_secs(160));
        // 10<<5 = 320 → capped at 300, and stays capped for higher counts.
        assert_eq!(delay(6), Duration::from_secs(300));
        assert_eq!(delay(50), Duration::from_secs(300));
    }
}
