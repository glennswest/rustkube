//! apimachinery: Shared types, traits, and error handling (K8s apimachinery-equivalent).
//!
//! Re-exports k8s-openapi types and defines internal abstractions
//! for the distributed store, watch events, RBAC, and certificates.

pub mod error;
pub mod store;
pub mod watch;
pub mod meta;
pub mod rbac;
pub mod certs;

pub use error::{Error, Result};

/// RustKube version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
