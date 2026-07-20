//! Shared recreation backoff for workload controllers.
//!
//! When a controller-owned pod fails and is replaced, an immediately-failing
//! image would otherwise churn create/delete every reconcile. This tracks a
//! per-key exponential backoff (CrashLoopBackOff-style) so replacements are
//! spaced out: `base * 2^(failures-1)`, capped at `max`. Keys are chosen by the
//! caller — a ReplicaSet uid, or a DaemonSet-uid/node pair.
//!
//! `base`/`max` are per-instance so a controller can mirror its upstream
//! counterpart — e.g. the DaemonSet controller uses k8s's `failedPodsBackoff`
//! window (1s → 15min) via [`CreateBackoff::with_params`].

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEFAULT_BASE_SECS: u64 = 10;
const DEFAULT_MAX_SECS: u64 = 300;

struct Entry {
    failures: u32,
    next: Instant,
}

/// Per-key create backoff, shared across reconciles on a controller.
pub struct CreateBackoff {
    base_secs: u64,
    max_secs: u64,
    inner: Mutex<HashMap<String, Entry>>,
}

impl Default for CreateBackoff {
    fn default() -> Self {
        Self::new()
    }
}

impl CreateBackoff {
    /// Default window (`base` 10s, `max` 300s).
    pub fn new() -> Self {
        Self::with_params(
            Duration::from_secs(DEFAULT_BASE_SECS),
            Duration::from_secs(DEFAULT_MAX_SECS),
        )
    }

    /// Explicit exponential window. `base` is the first-failure delay, `max` the
    /// cap. Both are floored at 1s.
    pub fn with_params(base: Duration, max: Duration) -> Self {
        Self {
            base_secs: base.as_secs().max(1),
            max_secs: max.as_secs().max(1),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Record an observed failure for `key`, widening its backoff window.
    pub fn record_failure(&self, key: &str, now: Instant) {
        let mut m = self.inner.lock().unwrap();
        let e = m.entry(key.to_string()).or_insert(Entry {
            failures: 0,
            next: now,
        });
        e.failures = e.failures.saturating_add(1);
        e.next = now + self.delay(e.failures);
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

    /// Exponential delay: `base * 2^(failures-1)`, capped at `max`. `failures` is
    /// 1-based (first failure → `base`). Overflow-safe, so a large `max` (e.g.
    /// k8s's 15min) is reached rather than clamped early.
    fn delay(&self, failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(32);
        let secs = self
            .base_secs
            .checked_shl(shift)
            .unwrap_or(u64::MAX)
            .min(self.max_secs);
        Duration::from_secs(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_delay_grows_then_caps() {
        let b = CreateBackoff::new();
        assert_eq!(b.delay(1), Duration::from_secs(10));
        assert_eq!(b.delay(2), Duration::from_secs(20));
        assert_eq!(b.delay(5), Duration::from_secs(160));
        // 10<<5 = 320 → capped at 300, and stays capped for higher counts.
        assert_eq!(b.delay(6), Duration::from_secs(300));
        assert_eq!(b.delay(50), Duration::from_secs(300));
    }

    #[test]
    fn k8s_daemonset_window_reaches_15min() {
        // failedPodsBackoff: base 1s doubling to a 15min cap (matches upstream).
        let b = CreateBackoff::with_params(Duration::from_secs(1), Duration::from_secs(900));
        assert_eq!(b.delay(1), Duration::from_secs(1));
        assert_eq!(b.delay(2), Duration::from_secs(2));
        assert_eq!(b.delay(10), Duration::from_secs(512));
        // 1<<10 = 1024 → capped at 900, and stays there (no early clamp, no overflow).
        assert_eq!(b.delay(11), Duration::from_secs(900));
        assert_eq!(b.delay(u32::MAX), Duration::from_secs(900));
    }
}
