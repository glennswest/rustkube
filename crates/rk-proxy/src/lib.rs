//! rk-proxy: Service proxy routing traffic to backend pods.
//!
//! Phase 1: iptables DNAT rules for ClusterIP/NodePort services.
//! Phase 2: eBPF-based packet redirection via aya for high performance.
