# CLAUDE.md — RustKube Project Instructions

## Project Overview

RustKube is a complete, K8s API-compatible container orchestrator in Rust. Wire-compatible with kubectl, helm, and existing YAML manifests. Target scale: 100–1000+ nodes.

**Key architectural decision:** RustKube uses the **kube architecture** — the API
server talks to an *external* datastore over the etcd v3 gRPC wire protocol, exactly
like upstream `kube-apiserver` → etcd. The datastore is **fastetcd** (`../fastetcd`),
a Rust, wire-compatible etcd v3 replacement.

- `storage::EtcdStore` → `KvStore` impl over the `etcd-client` crate (talks to fastetcd)
- Endpoints are supplied via `--etcd-servers` (required); optional mutual TLS via
  `--etcd-cacert` / `--etcd-cert` / `--etcd-key`
- **No embedded store and no stormforce dependency** — the earlier `stormforce-kv`
  embedded store was removed in favor of external fastetcd.

## Build & Test

```bash
cargo check           # type-check all crates
cargo build           # debug build
cargo test            # run all tests
cargo clippy          # lint
```

## Workspace Structure

Upstream-shaped: thin `cmd/<component>` binaries over `pkg/<lib>` libraries.
This repo is the control plane only.

```
cmd/
  kube-apiserver/            binary (main.rs) → apiserver
  kube-controller-manager/   binary → controller-manager
  kube-scheduler/            binary → scheduler
pkg/
  apimachinery/     Shared types (k8s-openapi re-exports), errors, traits, RBAC, cert utils
  storage/          Datastore client — etcd v3 gRPC (etcd-client) to external fastetcd
  apiserver/        K8s REST API (axum), auth, admission, watch cache, API groups (lib)
  scheduler/        Pod scheduling (filter, score, bind)
  controller-manager/ Built-in controllers (Deployment, ReplicaSet, Service, Namespace, etc.)
  cloud/            Cloud controller manager framework
```

Node level (kubelet, kube-proxy, cni) → separate repo `rustkube-node`.
DNS is external (microdns). Datastore is external (fastetcd).

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
- etcd-client 0.14 (external datastore client → fastetcd, etcd v3 wire protocol)

## Current Version: `v0.7.10`

## Work Plan

### Phase 0: Repository Setup (COMPLETE)
- [x] Git init, GitHub repo, workspace scaffold
- [x] All 10 crates compiling
- [x] Stormforce integration (kv, raft, vault, registry, security)

### Phase 1: Minimal Viable Cluster (COMPLETE)

**rk-core — Shared types and utilities**
- [x] Error types (NotFound, AlreadyExists, Conflict, Gone, Unauthorized, Forbidden, Invalid)
- [x] KvStore trait definition (get, put, delete, list, watch, lease, compact)
- [x] WatchEvent types (Added, Modified, Deleted, Bookmark)
- [x] Metadata helpers (resourceVersion ↔ revision)
- [x] RBAC types (AuthorizationRequest, AuthorizationDecision)
- [x] Certificate utilities (rcgen TLS cert generation)
- [x] VERSION constant

**rk-store — KvStore implementation (stormforce-kv wrapper)**
- [x] StormforceStore implementing KvStore trait
- [x] Get, put, delete with revision tracking
- [x] List with prefix scan and pagination (continue tokens)
- [x] Watch with historical replay + live streaming
- [x] Compare-and-swap transactions (optimistic locking)
- [x] Lease management (grant, keepalive, revoke)
- [x] Revision compaction
- [x] Single-node in-process mode for testing
- [x] 3 integration tests (CRUD, CAS, lease)

**rk-apiserver — K8s REST API server (axum 0.8)**
- [x] Core v1 resources: namespaces, nodes, pods, services, endpoints, configmaps, secrets, serviceaccounts, events, PVs, PVCs
- [x] Apps v1 resources: deployments, replicasets, statefulsets, daemonsets
- [x] Batch v1 resources: jobs, cronjobs
- [x] Coordination v1: leases
- [x] RBAC v1: clusterroles, clusterrolebindings, roles, rolebindings
- [x] apiextensions.k8s.io/v1: customresourcedefinitions (CRD support)
- [x] RustKube v1alpha1: podmigrations
- [x] Generic CRUD handlers (GET, LIST, POST, PUT, DELETE) for cluster + namespace scoped
- [x] Watch streaming (chunked JSON, WatchEvent protocol)
- [x] Label selectors (=, !=, in, notin, exists, !key)
- [x] Field selectors (metadata.name, spec.nodeName, status.phase, etc.)
- [x] Pagination (limit, continue tokens)
- [x] API discovery (/api, /apis, /version, /healthz, /livez, /readyz, per-group resource lists)
- [x] Dynamic API discovery (CRD groups included in /apis)
- [x] JWT bearer token authentication (HMAC-SHA256)
- [x] RBAC authorization engine (ClusterRole/RoleBindings, rule matching, wildcards)
- [x] Bootstrap RBAC (cluster-admin, system:masters, dev-mode anonymous admin)
- [x] Bootstrap namespaces (default, kube-system, kube-public, kube-node-lease)
- [x] CRD registry (dynamic resource registration, catch-all routes)
- [x] K8s Status error responses (404, 409, 422, 500, 410, 401, 403)
- [x] ResourceVersion tracking on all mutations
- [x] 6 selector unit tests

**rk-scheduler — Pod scheduling**
- [x] Filter plugins: NodeReady, Unschedulable, TaintToleration, NodeSelector, ResourceFit
- [x] Score plugins: LeastRequested, ImageLocality, NodeAffinity
- [x] Scheduling loop (watch unscheduled pods, filter, score, bind)
- [x] CPU/memory resource parsing (millicores, Ki/Mi/Gi)
- [x] Plugin trait framework for extensibility
- [x] 5 unit tests

**rk-controllers — 10 built-in controllers**
- [x] Deployment controller (ReplicaSet management, rolling updates, template hashing)
- [x] ReplicaSet controller (pod scaling, owner references, LIFO deletion)
- [x] Service controller (Endpoints from selector-matched pods)
- [x] Namespace controller (default ServiceAccount creation)
- [x] Node lifecycle controller (Lease heartbeat monitoring, NotReady marking)
- [x] Migration controller (PodMigration CRD state machine)
- [x] StatefulSet controller (ordered creation/deletion by ordinal, Ready gating)
- [x] DaemonSet controller (one pod per Ready node, bypasses scheduler)
- [x] Job controller (completions, parallelism, backoff limits, active deadlines)
- [x] CronJob controller (5-field cron parser, Allow/Forbid/Replace concurrency, history limits)
- [x] Controller manager (JoinSet-based concurrent runner)
- [x] ApiClient (HTTP client for apiserver communication)
- [x] 4 cron parser unit tests

**rk-kubelet — Node agent**
- [x] CRI trait definitions (RuntimeService, ImageService) matching K8s CRI v1
- [x] Pod lifecycle state machine (Pending → Running → Succeeded/Failed)
- [x] Health probes: HTTP GET, TCP socket, exec, gRPC
- [x] Node registration and Lease heartbeat reporting
- [x] System resource reporting (CPU, memory, conditions)
- [x] Native container runtime (youki libcontainer, OCI spec builder)
- [x] VM runtime (cloud-hypervisor, QEMU, Firecracker, auto-detection)
- [x] CRI client (bridges to containerd/CRI-O via crictl)
- [x] CRIU checkpoint/restore for container migration
- [x] VM live migration (CH REST API, QEMU QMP, Firecracker snapshots)
- [x] Migration annotation handling (checkpoint, prepare-target, live-migrate, restore)
- [x] Node drain helper (PodMigration for all non-DaemonSet pods)
- [x] Cross-platform stubs for macOS development

**rk-proxy — Service proxy**
- [x] iptables DNAT for ClusterIP + NodePort
- [x] Service map (DashMap-based, session affinity)
- [x] Probabilistic load balancing (iptables statistic module)
- [x] IP masquerade rules
- [x] iptables-restore for atomic updates
- [x] Endpoints syncer (watches Services + Endpoints)
- [x] Cross-platform stubs for macOS development

**rk-dns — Cluster DNS (hickory-dns 0.25)**
- [x] A records for ClusterIP services
- [x] A records for headless services (pod IPs)
- [x] SRV records for named service ports
- [x] PTR records for reverse DNS
- [x] Pod DNS (`<ip-dashed>.namespace.pod.cluster.local`)
- [x] Hostname-based DNS for stateful pods
- [x] UDP + TCP listeners
- [x] Background sync from API server
- [x] 2 unit tests

**rk-cni — CNI plugins**
- [x] CNI v1.0 spec types (config, result, error)
- [x] Host-local IPAM with disk-persisted allocations
- [x] Bridge plugin (veth pair, netns, IP assignment, routing)
- [x] VXLAN overlay (VTEP creation, FDB entries, peer routes)
- [x] IP masquerading
- [x] Cross-platform stubs for macOS development
- [x] 2 IPAM unit tests

**rk-cloud — Cloud controller manager**
- [x] CloudProvider trait (node addresses, zones, load balancers, routes)
- [x] NoopCloudProvider for bare-metal/dev
- [x] CloudControllerManager with reconciliation loops

### Phase 2: Production Features (COMPLETE)
- [x] Status subresource endpoints (GET/PUT/PATCH for all resource types)
- [ ] TLS listener wiring (config fields exist, listener not connected)
- [ ] ServiceAccount token generation (JWT signing key ready, SA token creation not wired)
- [x] Admission webhooks (mutating + validating chains, JSON patch, rule matching)
- [x] CSI volume support (Identity, Node, Controller traits, Unix socket client)
- [x] NetworkPolicy enforcement (CIDR matching, iptables rule gen, ingress/egress eval)

### Phase 3: Advanced (COMPLETE)
- [x] eBPF service proxy (BPF map types, service dispatch stubs, feature-gated)
- [x] eBPF CNI encap/decap (VXLAN overlay, peer management, feature-gated)
- [x] DNS upstream forwarding (round-robin, UDP, configurable timeout)
- [x] HPA controller (15s interval, velocity-limited scaling, stabilization window)
- [x] Gateway API (GatewayClass, Gateway, HTTPRoute controllers)
- [x] Full scheduler framework (plugins, preemption with priority-based eviction)
- [x] API aggregation layer (APIService registry, request proxying)
- [x] Cloud provider controllers (CloudProvider trait, noop provider, controller manager)

### Phase 4: Scale & Conformance
- [ ] 1000+ node testing
- [ ] K8s conformance test suite
- [ ] ARM64 cross-compile verification
- [ ] MikroTik minimal build verification

## Release History

| Version | Date | Summary |
|---------|------|---------|
| v0.7.10 | 2026-07-18 | CR PATCH (merge/JSON-patch/apply) + CR `/status` subresource; PATCH on built-in resources (#23) |
| v0.7.9 | 2026-07-18 | SA token auth: stable RS256 signing keypair across replicas (#11, #29) + default `kubernetes` Service/Endpoints (#30) |
| v0.7.8 | 2026-07-18 | Namespace deletion cascade — graceful Terminating + `/finalize` + controller purges contained resources (#28) |
| v0.7.7 | 2026-07-17 | Fix ReplicaSet/DaemonSet unbounded pod storm — GC terminal pods + exponential recreate backoff (#27) |
| v0.7.6 | 2026-07-17 | Fix LIST pagination hang — percent-decode `continue` token + label/field selectors in query parser |
| v0.7.5 | 2026-07-16 | Serve EndpointSlices (discovery.k8s.io/v1) in apiserver + controller-manager (#22) |
| v0.7.4 | 2026-07-16 | Fix CRD endpoints 500 — drop redundant apiextensions route (#21) |
| v0.7.3 | 2026-07-16 | Watch-cache stall re-seed (#18); pin fastetcd v0.8.2 (#8) |
| v0.3.0 | 2026-03-19 | Phase 2/3 — status subresources, admission, CSI, netpol, eBPF, HPA, Gateway, aggregation, cloud |
| v0.2.0 | 2026-03-18 | Label/field selectors, auth/RBAC, workload controllers, CRD support |
| v0.1.0 | 2026-03-17 | Initial scaffold — all 10 crates fully implemented |
