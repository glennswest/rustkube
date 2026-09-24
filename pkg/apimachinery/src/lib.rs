//! apimachinery: shared pieces every component uses.
//!
//! The error type, the `KvStore` trait and watch events, the protobuf wire
//! codec, metrics, quantities, selectors, taints, cron parsing and startup
//! waiting. No k8s-openapi types: objects are `serde_json::Value` throughout.

pub mod error;
pub mod store;
pub mod watch;
pub mod meta;
pub mod rbac;
pub mod certs;
pub mod cron;
pub mod kubevirt;
pub mod taint;
pub mod metrics;
pub mod protobuf;
pub mod quantity;
pub mod selector;
pub mod startup;

pub use error::{Error, Result};

/// RustKube version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
