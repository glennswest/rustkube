//! Scheduling plugin framework (Phase 3).
//!
//! Defines the plugin interfaces for the full scheduling framework.
//! Phase 1 uses hardcoded filter/score functions; this module will
//! provide the plugin registry for Phase 3.

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
