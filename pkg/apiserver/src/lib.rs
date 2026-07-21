//! rk-apiserver: Kubernetes-compatible REST API server.
//!
//! Serves the full K8s REST API via axum. Wire-compatible with kubectl,
//! helm, and existing K8s client libraries.

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
pub mod protobuf_mw;
pub mod rbac_engine;
pub mod selector;
pub mod server;
pub mod storage;
pub mod tls;
pub mod watch;
pub mod watch_cache;

pub use config::ApiServerConfig;
pub use server::run;
