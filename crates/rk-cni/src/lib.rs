//! rk-cni: CNI plugin binaries for pod network setup.
//!
//! Implements bridge, host-local IPAM, loopback, and portmap plugins.
//! Phase 1: VXLAN overlay for cross-node pod traffic.
//! Phase 2: eBPF-based encap/decap.
