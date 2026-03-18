# RustKube

A complete, K8s API-compatible container orchestrator written in Rust.

## Overview

RustKube is a full Kubernetes replacement built from the ground up in Rust for performance, safety, and minimal resource usage. It is wire-compatible with `kubectl`, `helm`, and existing Kubernetes YAML manifests.

**Target scale:** 100–1000+ nodes.

## Architecture

| Crate | Purpose |
|-------|---------|
| `rk-core` | Shared types (re-exports k8s-openapi), error handling, traits |
| `rk-store` | Distributed KV store (openraft 0.9 + RocksDB), replacing etcd |
| `rk-apiserver` | K8s REST API server (axum), auth, admission, watch cache |
| `rk-scheduler` | Pod scheduling framework (filter, score, bind) |
| `rk-controllers` | Built-in controllers (Deployment, ReplicaSet, Service, etc.) |
| `rk-kubelet` | Node agent, CRI client, pod lifecycle, health probes |
| `rk-proxy` | Service proxy (iptables fallback, eBPF primary) |
| `rk-dns` | Cluster DNS (hickory-dns, watches Services/Endpoints) |
| `rk-cni` | CNI plugins (bridge, host-local, VXLAN overlay) |
| `rk-cloud` | Cloud controller manager framework |

## Key Design Decisions

- **Embedded store** — openraft + RocksDB co-located with API server (like k3s, but linearizable). Separate deployment for large clusters.
- **Single binary option** — All control plane components in one process behind feature flags.
- **CRI reuse** — Uses CRI gRPC to talk to existing container runtimes (containerd, CRI-O).
- **eBPF-first networking** — iptables fallback for Phase 1, aya-based eBPF from Phase 2.
- **k8s-openapi for types** — Exact wire-level compatibility with kubectl.
- **kube-rs for controllers** — Mature controller runtime for watch/reconcile loops.
- **rustls only** — No OpenSSL. Fully static musl binaries.

## Build

```bash
# Full build
cargo build --release

# Static musl build
cargo build --release --target x86_64-unknown-linux-musl
```

## Test

```bash
cargo test
```

## Project Status

**Phase 0** — Repository setup and workspace scaffolding.

## License

Apache-2.0
