//! rk-scheduler: Pod scheduling framework.
//!
//! Watches for unscheduled pods and assigns them to nodes based on
//! resource availability, taints/tolerations, affinity, and scoring plugins.
