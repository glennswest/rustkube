//! rk-controllers: Built-in Kubernetes controllers.
//!
//! Reconciliation loops that drive cluster state toward desired state.
//! Each controller watches resources via the API server and creates/updates/deletes
//! dependent resources to match the desired spec.

pub mod deployment;
pub mod migration;
pub mod replicaset;
pub mod service;
pub mod namespace;
pub mod node;
pub mod runner;

pub use runner::ControllerManager;
