//! Plugin traits — unused.
//!
//! Nothing implements or calls these. The scheduler calls the fixed functions
//! in `filter` and `score` directly; there is no plugin registry.

use serde_json::Value;

/// A filter plugin decides if a node is feasible for a pod.
pub trait FilterPlugin: Send + Sync {
    fn name(&self) -> &str;
    fn filter(&self, pod: &Value, node: &Value) -> bool;
}

/// A score plugin ranks feasible nodes. Returns 0-100.
pub trait ScorePlugin: Send + Sync {
    fn name(&self) -> &str;
    fn score(&self, pod: &Value, node: &Value) -> i64;
    fn weight(&self) -> i64 {
        1
    }
}
