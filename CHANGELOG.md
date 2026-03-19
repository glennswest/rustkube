# Changelog

## [v0.3.0] — 2026-03-19

### Added
- **Status subresource endpoints** — GET/PUT/PATCH `/status` for all resource types
  - Cluster-scoped and namespace-scoped status handlers with merge-patch support
- **Admission webhooks** — mutating and validating webhook chain
  - JSON patch (add, remove, replace) application from webhook responses
  - Rule matching against webhook configurations (apiGroups, resources, operations)
  - Dynamic webhook loading from storage
- **CSI volume support** — Container Storage Interface plugin framework
  - Identity, Node, and Controller service traits
  - Unix socket CSI client for driver communication
  - Volume setup/teardown helpers for kubelet integration
- **NetworkPolicy enforcement** — network policy engine for pod traffic control
  - CIDR matching, pod/namespace selector matching
  - Ingress/egress rule evaluation with port filtering
  - iptables rule generation from NetworkPolicy resources
- **eBPF service proxy** — BPF-based service dispatch (Linux-only, feature-gated)
  - Service map with endpoint tracking, protocol-aware routing
  - BPF map types for O(1) service VIP resolution
- **eBPF CNI encap/decap** — VXLAN overlay with BPF (Linux-only, feature-gated)
  - Peer management, bulk peer updates, VNI configuration
  - Attach to network interfaces for encapsulation/decapsulation
- **DNS upstream forwarder** — forward non-cluster queries to upstream resolvers
  - Round-robin server selection, UDP transport, configurable timeout
- **HPA controller** — Horizontal Pod Autoscaler (15s reconcile interval)
  - Scales Deployments, ReplicaSets, StatefulSets
  - CPU utilization metrics, velocity-limited scaling (doubles/halves)
  - stabilization window for scale-down cooldown
- **Gateway API controller** — Gateway API v1 support
  - GatewayClass, Gateway, HTTPRoute reconciliation
  - Listener status tracking, route-gateway binding
- **Scheduler preemption** — priority-based pod eviction
  - Finds preemption candidates when scheduling fails
  - Minimizes number of victims, respects PDB-like constraints
  - Resource parsing for CPU (millicores) and memory (bytes)
- **API aggregation layer** — external API server registration
  - APIService registry (register, unregister, lookup)
  - Request proxying to aggregated API servers
  - Availability tracking from APIService status conditions
- **Cloud provider framework** — pluggable cloud controller interface
  - CloudProvider trait (node addresses, zones, load balancers, routes)
  - NoopCloudProvider for bare-metal/dev environments
  - CloudControllerManager with reconciliation loops
- New API groups: `autoscaling/v2`, `networking.k8s.io/v1`,
  `admissionregistration.k8s.io/v1`, `gateway.networking.k8s.io/v1`,
  `apiregistration.k8s.io/v1`
- New list kind mappings for all new resource types

### Changed
- Controller manager now runs 12 controllers (was 10)
- ApiError gains `forbidden()`, `unauthorized()` constructors and `Display` impl
- rk-cni gains tokio and anyhow dependencies for async eBPF module

## [v0.2.0] — 2026-03-18

### Added
- Label and field selector parsing and filtering for list/watch operations
  - Label selectors: `=`, `!=`, `in`, `notin`, exists, `!key`
  - Field selectors: `metadata.name`, `metadata.namespace`, `spec.nodeName`, `status.phase`
  - Selectors applied in list handlers and watch event streams
- Authentication middleware (JWT bearer token with HMAC-SHA256 signing)
- RBAC authorization engine
  - Evaluates ClusterRoleBindings and RoleBindings against ClusterRoles and Roles
  - system:masters group always has full access
  - Bootstrap creates cluster-admin role and system:masters binding
  - Dev-mode anonymous admin access for kubectl without certs
- StatefulSet controller — ordered creation/deletion by ordinal, waits for Ready
- DaemonSet controller — one pod per Ready node, bypasses scheduler via nodeName
- Job controller — tracks completions, parallelism, backoff limits, active deadlines
- CronJob controller — 5-field cron parser, concurrency policies (Allow/Forbid/Replace), history limits
- CRD support — dynamic resource registration via CustomResourceDefinition
  - CrdRegistry for in-memory tracking of registered custom resources
  - Catch-all routes serve custom resources through generic storage layer
  - Dynamic API discovery includes CRD groups
- `batch/v1` API group with jobs and cronjobs resources
- `apiextensions.k8s.io/v1` API group for CRD management

### Changed
- Controller manager now runs 10 controllers (was 6)
- API group discovery is now dynamic (includes CRD groups)
- AppState includes CrdRegistry for dynamic resource support
- ApiServerConfig adds `service_account_key` and `anonymous_auth` fields

## [v0.1.0] — 2026-03-17

### Added
- Pod migration controller — runtime-aware pod migration between nodes
  - MigrationService trait with per-runtime strategies (CRIU, live migrate, snapshot, evacuate)
  - CRIU checkpoint/restore for native containers (~100ms downtime)
  - VM live migration helpers: cloud-hypervisor REST API, QEMU QMP, Firecracker snapshot
  - MigrationService implemented for NativeRuntime, VmRuntime, CriClient
  - PodMigration custom resource (rustkube.io/v1alpha1) with state machine controller
  - Migration state machine: Pending -> Checkpointing -> Transferring -> Restoring -> Verifying -> Completed
  - Kubelet migration annotation handling (checkpoint, prepare-target, live-migrate, restore)
  - Node drain helper (creates PodMigration for all non-DaemonSet pods)
  - Non-Linux stubs for macOS development
- rk-store — KvStore trait implementation wrapping stormforce-kv (CRUD, CAS, watch, lease)
- rk-apiserver — Full K8s REST API server (axum 0.8)
  - Discovery: /api, /apis, /version, /healthz, /livez, /readyz
  - Core v1: namespaces, nodes, pods, services, configmaps, secrets, endpoints, serviceaccounts, events, PVs, PVCs
  - Apps v1: deployments, replicasets, statefulsets, daemonsets
  - Coordination v1: leases
  - RBAC v1: roles, clusterroles, bindings
  - Generic CRUD handlers (GET/LIST/POST/PUT/DELETE) with watch streaming
  - ResourceVersion tracking, K8s Status error responses
  - Bootstrap namespaces (default, kube-system, kube-public, kube-node-lease)
- rustkube-apiserver binary with clap CLI
- kubectl verified: get ns/nodes/pods, version, api-resources all working
- rk-controllers — 5 built-in controllers
  - Deployment: creates/manages ReplicaSets, rolling updates, scale up/down
  - ReplicaSet: creates/deletes Pods to maintain replica count
  - Service: creates/updates Endpoints from selector-matched pods
  - Namespace: ensures default ServiceAccount in each namespace
  - Node Lifecycle: monitors Lease heartbeats, marks nodes NotReady
- rk-scheduler — pod scheduling with filter/score framework
  - Filters: NodeReady, Unschedulable, TaintToleration, NodeSelector, ResourceFit
  - Scores: LeastRequested, ImageLocality, NodeAffinity
  - Plugin trait framework for Phase 3 extensibility
- Single-binary control plane (rustkube) — apiserver + controllers + scheduler
- End-to-end verified: Deployment -> ReplicaSet -> 3 Pods -> scheduled to node
- rk-kubelet — Node agent with CRI, pod lifecycle, health probes
  - CRI trait definitions (RuntimeService, ImageService) matching K8s CRI v1
  - Pod lifecycle manager (Pending -> Running -> Succeeded/Failed)
  - Health probes: HTTP GET, TCP connect, exec, gRPC
  - Node registration and Lease heartbeat reporting
- rk-proxy — Service proxy with iptables DNAT
  - Service map tracking ClusterIP -> pod endpoint backends
  - iptables rule generation with probabilistic load balancing
  - NodePort support, session affinity, IP masquerade
- rk-dns — Cluster DNS server (hickory-dns 0.25)
  - A records for ClusterIP services and headless pod IPs
  - SRV records for named service ports
  - PTR records for reverse DNS
  - Pod DNS: `<ip-dashed>.namespace.pod.cluster.local`
- rk-cni — CNI plugins for pod networking
  - CNI v1.0 spec types (config, result, error)
  - Host-local IPAM with disk-persisted allocations
  - Bridge plugin: veth pair, netns, IP assignment, routing
  - VXLAN overlay: VTEP creation, FDB entries, peer routes
- Native container runtime via youki libcontainer
  - NativeRuntime: OCI container lifecycle without containerd/runc
  - Full OCI spec builder (rootfs, process, mounts, cgroups v2)
  - NativeImageService: image pulls via skopeo
  - Architecture: kubelet -> libcontainer -> kernel (no Go)
- VM runtime for microVM-isolated pods
  - VmRuntime: each pod sandbox runs as a microVM with own kernel
  - Supports cloud-hypervisor (Rust-native), Firecracker, QEMU/KVM
  - Per-pod VM config via annotations (rustkube.io/vm-*)
  - virtiofs volume sharing, guest agent exec, SSH fallback
  - Runtime selection: `--runtime=native|vm|cri --vmm=auto|cloud-hypervisor|qemu|firecracker`
- Initial repository setup — Cargo workspace with 10 member crates
