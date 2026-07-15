//! rk-controllers: Built-in Kubernetes controllers.
//!
//! Reconciliation loops that drive cluster state toward desired state.
//! Each controller watches resources via the API server and creates/updates/deletes
//! dependent resources to match the desired spec.

pub mod cronjob;
pub mod daemonset;
pub mod deployment;
pub mod gateway;
pub mod hpa;
pub mod job;
pub mod leaderelection;
pub mod migration;
pub mod namespace;
pub mod node;
pub mod replicaset;
pub mod runner;
pub mod service;
pub mod statefulset;

pub use runner::{ClientConfig, ControllerManager};
