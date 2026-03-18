//! rk-core: Shared types, traits, and error handling for RustKube.
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
