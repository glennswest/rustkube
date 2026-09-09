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

- k8s-openapi 0.24 (types); reports K8s 1.36 API posture (#37); kube-rs 0.99
- axum 0.8, tower 0.5, hyper 1.x
- rustls 0.23 (no OpenSSL — static musl binaries)
- tonic 0.12, prost 0.13 (CRI gRPC)
- hickory-dns 0.25 (cluster DNS)
- etcd-client 0.14 (external datastore client → fastetcd, etcd v3 wire protocol)

## Current Version: `v0.12.0`

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
- [x] TLS listener wiring — rustls serving, x509 client auth, and (v0.8.0) a
      swappable cert resolver so a renewed certificate is picked up without a
      restart (#20)
- [x] ServiceAccount token generation (`/serviceaccounts/{name}/token`, RS256,
      stable signing keypair across replicas — v0.7.9)
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

### Storage (v0.8.0)
- [x] PV/PVC binding, protection finalizers, phases, reclaim, events (#56)
- [x] Attach/detach — `VolumeAttachment` for drivers that require it
- [x] Volume-aware scheduling — PV `nodeAffinity`, `selected-node`,
      `CSIStorageCapacity`
- [ ] Volume expansion — `status.allocatedResources` + resize conditions (#63)
- [ ] Snapshots — the external-snapshotter CRDs and controller (#64)
- [ ] `ReadWriteOncePod` enforcement (#65)
- See [docs/storage.md](docs/storage.md) for the contract with stormblock,
  sbregistry and stormblock-csi. **rustkube provisions nothing itself.**

### Phase 4: Scale & Conformance
- [ ] 1000+ node testing (#66) — the controllers list everything every tick,
      which is what will break first
- [ ] K8s conformance test suite (#67)
- [ ] ARM64 cross-compile verification + MikroTik minimal build (#68) — one
      question: CI builds x86_64 musl only, so nothing knows if ARM64 compiles

### `oc` compatibility — the surface that drives completeness

**[docs/oc-compatibility.md](docs/oc-compatibility.md)** is the reference: what
`oc` can ask a cluster to do, and therefore what this apiserver has to answer
for. It is the specification for "complete", not a wish list — a verb that is
not answered is a gap whether or not anyone has hit it yet.

Work it as a checklist against a live cluster rather than by reading: `oc
<verb> --help` on the target is authoritative, and the runbook's Part II is a
verification pass that should collapse into one script with an exit code.

Known state on 2026-08-28:

- [x] `oc logs` — apiserver `pods/log` proxying to the kubelet's
      `/containerLogs`, with TokenReview so anything can authenticate to a
      kubelet at all (#54). `container`, `tailLines`, `previous` work;
      `timestamps`/`sinceSeconds`/`sinceTime` are inert because stormpump logs
      are raw by design. `follow` streams end to end as of 2026-09-09: the
      apiserver stopped reading the body to a String (2026-09-08) and the
      kubelet stopped answering with a snapshot and closing (rustkube-node#34).
      `limitBytes` is honored on the node. A named container may be an init or
      ephemeral one — a failed init container's log, and a sidecar's, were
      refused as "not valid for pod" until v0.8.1 (#55).
- [x] `oc exec`, `attach`, `port-forward` — and therefore `rsh`, `cp`, `rsync`
      and `debug`, which are those three plus argument handling (#42). Proxied
      to the kubelet as a **transparent connection upgrade**: nothing parses
      SPDY or WebSocket frames, the client's headers go up verbatim and the
      kubelet's 101 comes back verbatim, so both protocols work from one
      implementation. The one translation is the query — `stdin/stdout/stderr`
      here are `input/output/error` on the kubelet.
      Cilium's CLI links client-go's `FallbackExecutor` (WebSocket first, SPDY
      second); both paths are tested.
- [ ] `oc adm` — largely unexamined (#69). Most of it is object edits that
      should already work; the known holes are `SubjectAccessReview` (the
      privileged sibling of #59), CSR `/approval`, and `metrics.k8s.io`.
- [ ] Routes, DeploymentConfig, ImageStream, BuildConfig, SCC — the
      genuinely OpenShift-only half. Whether these are in scope at all is a
      decision nobody has made (#70), and `route.openshift.io/v1` is already
      *served* with nothing routing for it, which is the inconsistency that
      forces the question; `oc` without them is `kubectl` with better
      ergonomics, which may be the right target.

## Release History

| Version | Date | Summary |
|---------|------|---------|
| v0.12.0 | 2026-09-09 | **Data loss fix**: controller list follows the `continue` token — past 500 objects of a kind the GC was deleting live objects whose owner fell beyond the first page (#66). `SubjectAccessReview` for `oc adm policy who-can` (#69) |
| v0.11.0 | 2026-09-09 | VirtualMachine controller + `start`/`stop`/`restart` verbs (#62). Fix: discovery paths matched by shape, so CRD groups stop 403-ing and the GC can finally see custom resources |
| v0.10.0 | 2026-09-09 | Serve `subresources.kubevirt.io/v1` console/vnc doors so `virtctl console` resolves — proxied node-ward through the kubelet, because stormvm mints only on loopback (#61) |
| v0.9.1 | 2026-09-09 | Security: bootstrap removes the `system:anonymous-admin` binding when the dev grant is off, so turning `--dev-anonymous-admin` off actually revokes it (#60) |
| v0.9.0 | 2026-09-09 | `authorization.k8s.io/v1` self-reviews so a console can ask what a user may see (#59), `system:authenticated` + `system:basic-user`. Security: a `nonResourceURLs` rule no longer granted every resource GET (anonymous could read secrets); a grouped namespaced path is no longer read as a namespace subresource |
| v0.8.1 | 2026-09-09 | `pods/log` and `pods/exec` accept an init or ephemeral container name — a failed init container's log and a shell in a sidecar were refused as "not valid for pod" (#55, #54) |
| v0.8.0 | 2026-09-08 | Storage: PV/PVC binding, attach/detach, volume-aware scheduling (#56). GC: foreground + orphan propagation, discovery-driven (#43). exec/attach/port-forward (#42) and the RBAC subresource hole they exposed. Static musl + `FROM scratch` images (#50). Upstream metric names everywhere (#51). Serving-cert hot reload + renewal (#20). Credential waiting instead of exit-1 at boot (#58) |
| v0.7.35 | 2026-07-21 | Add [profile.release] — opt3 + thin LTO + codegen-units=1, strip debuginfo, keep the symbol table so panics name functions (#49; the original entry said "keep line tables" — `strip = "debuginfo"` removes those, so backtraces give functions, not file:line) |
| v0.7.34 | 2026-07-21 | Serve events.k8s.io/v1 (translated to/from stored core/v1 Event) (#48); versioned control-plane container images CI (#46) |
| v0.7.33 | 2026-07-20 | Strategic-merge-patch honors patchMergeKey — node status.conditions upsert by type (not replaced), so nodes keep Ready after Cilium sets NetworkUnavailable (#47) |
| v0.7.32 | 2026-07-20 | Server-side apply managedFields: field-ownership tracking (fieldsV1), prune dropped fields, conflict detection (409 unless force) — completes SSA (#45) |
| v0.7.31 | 2026-07-20 | DaemonSet keys off node ELIGIBILITY not readiness (no churn on transient NotReady) + real status counts (#44); server-side apply upserts a missing object (#45) |
| v0.7.30 | 2026-07-20 | Proper DeleteOptions semantics: decode meta/v1 DeleteOptions (protobuf+JSON), honor preconditions(409)/dryRun/gracePeriod/finalizers/propagationPolicy — replaces the v0.7.27 skip |
| v0.7.29 | 2026-07-20 | CRD list/watch use the CRD real listKind (CiliumNetworkPolicyList) not {plural}List — Cilium agent CR informers sync (#39 chain) |
| v0.7.28 | 2026-07-20 | PartialObjectMetadata projection (as=PartialObjectMetadata) for list+watch — Cilium agent CRD metadata-informer syncs, goes Ready (#39 chain) |
| v0.7.27 | 2026-07-20 | protobuf mw: only transcode POST/PUT/PATCH bodies — DELETE carries DeleteOptions (no schema), was 415-ing helm/cilium uninstall |
| v0.7.26 | 2026-07-20 | JSON Patch: test-null against absent path holds (evanphx/k8s leniency) — unblocks cilium-operator node-taint CAS |
| v0.7.25 | 2026-07-20 | Watch BOOKMARK support: WatchList sendInitialEvents→initial-events-end bookmark + allowWatchBookmarks heartbeat — client-go informers sync, unblocks Cilium agent (#39) |
| v0.7.24 | 2026-07-19 | DaemonSet self-healing: delete+recreate Failed pods with random names (k8s generateName style) + failedPodsBackoff (1s→15min) — unblocks Cilium agent DS (#38) |
| v0.7.23 | 2026-07-19 | Report K8s 1.36 API posture (/version, discovery); 1.33-1.36 served group-versions unchanged, substantive deltas are node-side (#37) |
| v0.7.22 | 2026-07-19 | CRD establishing: created CRDs get status (acceptedNames + NamesAccepted/Established + storedVersions) so clients stop hanging (#36) |
| v0.7.21 | 2026-07-19 | CRITICAL: protobuf decode uses endpoint GVK when envelope TypeMeta is blank — typed client-go clients (cilium CRD create) now work (#34) |
| v0.7.20 | 2026-07-19 | Cert-expiry monitoring: apiserver_certificate_expiration_seconds metric + near-expiry warnings (#20 Phase 1a) |
| v0.7.19 | 2026-07-19 | CRITICAL: protobuf codec for apiextensions CRDs (schema union types) + policy/autoscaling/scheduling/admission/certificates — unblocks cilium CRD creation (#34) |
| v0.7.18 | 2026-07-19 | Node draining: policy/v1 PodDisruptionBudget + PDB-gated pod Eviction subresource (429) + PDB status controller (#7) |
| v0.7.17 | 2026-07-19 | Security hardening: refuse plain HTTP without --insecure; anonymous no longer cluster-admin unless --dev-anonymous-admin (#16) |
| v0.7.16 | 2026-07-19 | CRITICAL: resourceVersion sourced from store mod_revision, not stale baked-in JSON — fixes optimistic concurrency & all leader election (#33) |
| v0.7.15 | 2026-07-19 | Emit core/v1 Events (SuccessfulCreate/Delete, ScalingReplicaSet) + event TTL GC (#15); richer apiserver metrics — latency histogram, verb/resource/code, inflight (#13) |
| v0.7.14 | 2026-07-19 | client-go protobuf wire codec (application/vnd.kubernetes.protobuf), both directions — unblocks cilium-operator & all client-go controllers (#32) |
| v0.7.13 | 2026-07-19 | OpenAPI v3 paths declare GVK + fieldValidation so `kubectl apply` stops falling back to protobuf v2 (#31) |
| v0.7.12 | 2026-07-19 | Watch tombstones carry TypeMeta (fixes client-go informers); serve /openapi/v2+v3 so `kubectl apply` validates |
| v0.7.11 | 2026-07-18 | Serve storage.k8s.io/v1 — StorageClass, CSIDriver, CSINode, VolumeAttachment, CSIStorageCapacity (#24) |
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
