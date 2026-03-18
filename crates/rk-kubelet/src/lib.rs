//! rk-kubelet: Node agent managing pod lifecycle via CRI.
//!
//! Connects to container runtimes (containerd, CRI-O) via gRPC,
//! manages pod state machines, health probes, volumes, image pulls,
//! and reports node status via Lease heartbeats.

pub mod cri;
pub mod health;
pub mod kubelet;
pub mod node_status;
pub mod pod_manager;

pub use kubelet::Kubelet;
