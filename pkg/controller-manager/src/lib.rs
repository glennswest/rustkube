//! controller-manager: the built-in Kubernetes controllers.
//!
//! Reconciliation loops that drive cluster state toward desired state. Each
//! controller **polls**: on a fixed interval it lists the resources it owns
//! through the API server (following `continue` tokens to the end) and
//! creates, updates or deletes dependents to match. Nothing uses a watch, an
//! informer cache or a work queue (#66).

pub mod attachdetach;
pub mod backoff;
pub mod cronjob;
pub mod csr;
pub mod daemonset;
pub mod deployment;
pub mod events;
pub mod gateway;
pub mod gc;
pub mod hpa;
pub mod job;
pub mod leaderelection;
pub mod metrics_server;
pub mod migration;
pub mod namespace;
pub mod persistentvolume;
pub mod pdb;
pub mod node;
pub mod replicaset;
pub mod rollout;
pub mod runner;
pub mod service;
pub mod stormblock;
pub mod virtualmachine;
pub mod statefulset;

pub use runner::{ClientConfig, ControllerManager};
