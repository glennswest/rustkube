//! controller-manager: the built-in Kubernetes controllers.
//!
//! Shared revisioned watches enqueue deduplicated reconciliation passes.
//! The compatibility adapter retains authoritative paginated API reads;
//! indexed per-object workers and cache reads are tracked in #146. Timers
//! represent semantic deadlines or failure recovery, not successful idle work.

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
pub mod resourcequota;
pub mod rollout;
pub mod owned;
pub mod rootca;
pub mod runner;
pub mod endpointslicemirroring;
pub mod service;
pub mod stormblock;
pub mod virtualmachine;
pub mod vmilauncher;
pub mod vmimigration;
pub mod statefulset;

pub use runner::{ClientConfig, ControllerManager};
