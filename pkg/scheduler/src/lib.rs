//! rk-scheduler: Pod scheduling framework.
//!
//! Watches for unscheduled pods (empty spec.nodeName) and assigns them
//! to nodes based on resource availability, taints/tolerations, and scoring.

pub mod filter;
pub mod leaderelection;
pub mod metrics_server;
pub mod plugins;
pub mod preemption;
pub mod scheduler;
pub mod score;

pub use scheduler::Scheduler;
