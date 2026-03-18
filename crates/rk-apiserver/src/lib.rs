//! rk-apiserver: Kubernetes-compatible REST API server.
//!
//! Serves the full K8s REST API via axum. Wire-compatible with kubectl,
//! helm, and existing K8s client libraries.

pub mod config;
pub mod discovery;
pub mod error;
pub mod handlers;
pub mod server;
pub mod storage;
pub mod watch;

pub use config::ApiServerConfig;
pub use server::run;
