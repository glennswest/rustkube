//! rk-kubelet: Node agent managing pod lifecycle via CRI.
//!
//! Connects to container runtimes (containerd, CRI-O) via gRPC,
//! manages pod state machines, health probes, volumes, image pulls,
//! and reports node status via Lease heartbeats.
