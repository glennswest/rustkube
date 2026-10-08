//! apiserver: Kubernetes-compatible REST API server.
//!
//! Serves the Kubernetes REST API (core and built-in groups, CRDs,
//! subresources) via axum, wire-compatible with kubectl, helm and client-go.
//! Admission webhooks ([`admission`], #82) run on every write, with RBAC
//! escalation prevention ([`escalation`], #98) between the mutating and the
//! validating ones. Aggregation ([`aggregation`], #83) is a module that is not
//! wired into the request path.

pub mod admission;
pub mod aggregation;
pub mod apply;
pub mod builtin_admission;
pub mod auth;
pub mod compactor;
pub mod config;
pub mod control_plane_rbac;
pub mod crd;
pub mod discovery;
pub mod error;
pub mod escalation;
pub mod events;
pub mod eviction;
pub mod field_validation;
pub mod handlers;
pub mod limitranger;
pub mod manifests;
pub mod protobuf_mw;
pub mod rbac_engine;
pub mod requester;
pub mod resource_metrics;
pub mod schema;
pub mod selector;
pub mod node_port;
pub mod openapi_crd;
pub mod quota_admission;
pub mod service_ip;
pub mod server;
pub mod storage;
pub mod table;
#[cfg(test)]
pub(crate) mod test_store;
pub mod tls;
pub mod token_file;
pub mod watch;
pub mod watch_cache;

pub use config::ApiServerConfig;
pub use server::run;
