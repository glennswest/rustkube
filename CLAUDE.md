# CLAUDE.md — RustKube Project Instructions

## Project Overview

RustKube is a complete, K8s API-compatible container orchestrator in Rust. Wire-compatible with kubectl, helm, and existing YAML manifests. Target scale: 100–1000+ nodes.

**Key architectural decision:** RustKube uses **stormforce** crates (from `../stormforce`) as its infrastructure backbone:
- `stormforce-kv` → etcd-compatible MVCC KV store (watch, lease, Raft consensus)
- `stormforce-raft` → Consensus layer
- `stormforce-vault` → Secrets management and PKI/CA
- `stormforce-registry` → OCI container registry
- `stormforce-security` → TLS, SASL, ACLs

## Build & Test

```bash
cargo check           # type-check all crates
cargo build           # debug build
cargo test            # run all tests
cargo clippy          # lint
```

## Workspace Structure

```
crates/
  rk-core/          Shared types (k8s-openapi re-exports), errors, traits, RBAC, cert utils
  rk-store/         KV store adapter — wraps stormforce-kv for K8s key schema
  rk-apiserver/     K8s REST API (axum), auth, admission, watch cache, API groups
  rk-scheduler/     Pod scheduling (filter, score, bind)
  rk-controllers/   Built-in controllers (Deployment, ReplicaSet, Service, Namespace, etc.)
  rk-kubelet/       Node agent (CRI gRPC client, pod lifecycle, health probes)
  rk-proxy/         Service proxy (iptables Phase 1, eBPF Phase 2)
  rk-dns/           Cluster DNS (hickory-dns, watches Services/Endpoints)
  rk-cni/           CNI plugins (bridge, host-local, VXLAN)
  rk-cloud/         Cloud controller manager framework
```

## Version Locations

```
Cargo.toml → workspace.package.version
```

## Key Dependencies

- k8s-openapi 0.24 (K8s 1.32), kube-rs 0.99
- axum 0.8, tower 0.5, hyper 1.x
- rustls 0.23 (no OpenSSL — static musl binaries)
- tonic 0.12, prost 0.13 (CRI gRPC)
- hickory-dns 0.25 (cluster DNS)
- stormforce-kv 0.9 (etcd replacement)

## Current Version: `v0.2.0`

## Work Plan

### Phase 0: Repository Setup (COMPLETE)
- [x] Git init, GitHub repo, workspace scaffold
- [x] All 10 crates compiling
- [x] Stormforce integration (kv, raft, vault, registry, security)

### Phase 1: Minimal Viable Cluster (IN PROGRESS)
- [ ] rk-store: KvStore trait impl wrapping stormforce-kv KvEngine
- [ ] rk-apiserver: Core resource CRUD (namespaces, pods, services, configmaps, secrets, nodes)
- [x] rk-apiserver: Watch/list with resourceVersion, pagination, label/field selectors
- [x] rk-apiserver: Auth (JWT bearer, RBAC engine, anonymous fallback)
- [x] rk-apiserver: RBAC authorization (ClusterRole/RoleBindings, system:masters)
- [ ] rk-apiserver: API discovery (/api, /apis, /version, /healthz)
- [ ] rk-scheduler: Basic scheduling (filter by resources/taints, score by least-loaded, bind)
- [ ] rk-controllers: Deployment → ReplicaSet → Pod
- [ ] rk-controllers: Service → Endpoints
- [ ] rk-controllers: Namespace lifecycle
- [ ] rk-controllers: Node lifecycle (lease heartbeat)
- [ ] rk-kubelet: CRI gRPC client (proto gen from cri-api)
- [ ] rk-kubelet: Pod lifecycle state machine
- [ ] rk-kubelet: Health probes (HTTP, TCP, exec)
- [ ] rk-kubelet: Node status reporting (Lease heartbeat)
- [ ] rk-proxy: iptables DNAT for ClusterIP/NodePort
- [ ] rk-dns: A/SRV records for services
- [ ] rk-cni: Bridge + host-local IPAM + VXLAN overlay

### Completed Features
- [x] Pod migration controller (MigrationService trait, CRIU, VM live migrate, PodMigration CRD)

### Phase 2: Production Features
- [x] CRD support (dynamic resource registration, catch-all routes, dynamic discovery)
- [x] StatefulSet, DaemonSet, Job, CronJob controllers
- [ ] CSI volume support
- [ ] Admission webhooks (mutating + validating)
- [ ] eBPF proxy (aya)
- [ ] NetworkPolicy enforcement

### Phase 3: Advanced
- [ ] HPA, Gateway API
- [ ] Full scheduler framework (plugins, preemption)
- [ ] API aggregation layer

### Phase 4: Scale
- [ ] 1000+ node testing
- [ ] Cloud provider controllers
- [ ] K8s conformance test suite

## Release History

| Version | Date | Summary |
|---------|------|---------|
| v0.2.0 | 2026-03-18 | Label/field selectors, auth/RBAC, workload controllers, CRD support |
| v0.1.0 | 2026-03-17 | Initial scaffold — 10 crates, stormforce integration |
