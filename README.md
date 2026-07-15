# RustKube

A complete, K8s API-compatible container orchestrator written in Rust.

## Overview

RustKube is a full Kubernetes replacement built from the ground up in Rust for performance, safety, and minimal resource usage. It is wire-compatible with `kubectl`, `helm`, and existing Kubernetes YAML manifests.

**Target scale:** 100–1000+ nodes.

## Architecture

Upstream-shaped layout: thin `cmd/<component>` binaries over `pkg/<lib>`
libraries. This repo is the **control plane** — the node level lives in
[rustkube-node](https://github.com/glennswest/rustkube-node), the datastore in
[fastetcd](https://github.com/glennswest/fastetcd), and DNS in
[microdns](https://github.com/glennswest/microdns).

```
cmd/                              pkg/
  kube-apiserver/           →       apiserver/          K8s REST API (axum), auth, admission, watch cache
  kube-controller-manager/  →       controller-manager/ built-in controllers (Deployment, ReplicaSet, Service, …)
  kube-scheduler/           →       scheduler/          pod scheduling (filter, score, bind)
                                    apimachinery/       shared types (re-exports k8s-openapi), errors, traits
                                    storage/            datastore client — etcd v3 gRPC to external fastetcd
                                    cloud/              cloud controller manager framework
```

Binaries carry exact upstream names (`kube-apiserver`, `kube-controller-manager`,
`kube-scheduler`) — a drop-in control plane.

## Key Design Decisions

- **External datastore (kube architecture)** — the API server talks to a standalone [fastetcd](https://github.com/glennswest/fastetcd) over the etcd v3 gRPC wire protocol (`--etcd-servers`), exactly like upstream kube-apiserver → etcd. No embedded store.
- **Single binary option** — All control plane components (except the datastore) in one process behind feature flags.
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
