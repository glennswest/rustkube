//! apiserver: Kubernetes-compatible REST API server.
//!
//! Serves the Kubernetes REST API (core and built-in groups, CRDs,
//! subresources) via axum, wire-compatible with kubectl, helm and client-go.
//! Admission webhooks ([`admission`], #82) and aggregation ([`aggregation`],
//! #83) are implemented as modules but not wired into the request path.

pub mod admission;
pub mod aggregation;
pub mod apply;
pub mod builtin_admission;
pub mod auth;
pub mod config;
pub mod crd;
pub mod discovery;
pub mod error;
pub mod events;
pub mod eviction;
pub mod handlers;
pub mod manifests;
pub mod protobuf_mw;
pub mod rbac_engine;
pub mod selector;
pub mod service_ip;
pub mod server;
pub mod storage;
pub mod table;
#[cfg(test)]
pub(crate) mod test_store;
pub mod tls;
pub mod watch;
pub mod watch_cache;

pub use config::ApiServerConfig;
pub use server::run;
