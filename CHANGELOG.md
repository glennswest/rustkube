# Changelog

## [Unreleased]

### 2026-03-18
- **feat:** rk-store — KvStore trait implementation wrapping stormforce-kv (CRUD, CAS, watch, lease)
- **feat:** rk-apiserver — Full K8s REST API server (axum 0.8)
  - Discovery: /api, /apis, /version, /healthz, /livez, /readyz
  - Core v1: namespaces, nodes, pods, services, configmaps, secrets, endpoints, serviceaccounts, events, PVs, PVCs
  - Apps v1: deployments, replicasets, statefulsets, daemonsets
  - Coordination v1: leases
  - RBAC v1: roles, clusterroles, bindings
  - Generic CRUD handlers (GET/LIST/POST/PUT/DELETE) with watch streaming
  - ResourceVersion tracking, K8s Status error responses
  - Bootstrap namespaces (default, kube-system, kube-public, kube-node-lease)
- **feat:** rustkube-apiserver binary with clap CLI
- **feat:** kubectl verified: get ns/nodes/pods, version, api-resources all working
- **feat:** rk-controllers — 5 built-in controllers
  - Deployment: creates/manages ReplicaSets, rolling updates, scale up/down
  - ReplicaSet: creates/deletes Pods to maintain replica count
  - Service: creates/updates Endpoints from selector-matched pods
  - Namespace: ensures default ServiceAccount in each namespace
  - Node Lifecycle: monitors Lease heartbeats, marks nodes NotReady
- **feat:** rk-scheduler — pod scheduling with filter/score framework
  - Filters: NodeReady, Unschedulable, TaintToleration, NodeSelector, ResourceFit
  - Scores: LeastRequested, ImageLocality, NodeAffinity
  - Plugin trait framework for Phase 3 extensibility
- **feat:** Single-binary control plane (rustkube) — apiserver + controllers + scheduler
- **feat:** End-to-end verified: Deployment → ReplicaSet → 3 Pods → scheduled to node
- **feat:** rk-kubelet — Node agent with CRI, pod lifecycle, health probes
  - CRI trait definitions (RuntimeService, ImageService) matching K8s CRI v1
  - Pod lifecycle manager (Pending → Running → Succeeded/Failed)
  - Health probes: HTTP GET, TCP connect, exec, gRPC
  - Node registration and Lease heartbeat reporting
- **feat:** rk-proxy — Service proxy with iptables DNAT
  - Service map tracking ClusterIP → pod endpoint backends
  - iptables rule generation with probabilistic load balancing
  - NodePort support, session affinity, IP masquerade
- **feat:** rk-dns — Cluster DNS server (hickory-dns 0.25)
  - A records for ClusterIP services and headless pod IPs
  - SRV records for named service ports
  - PTR records for reverse DNS
  - Pod DNS: `<ip-dashed>.namespace.pod.cluster.local`
- **feat:** rk-cni — CNI plugins for pod networking
  - CNI v1.0 spec types (config, result, error)
  - Host-local IPAM with disk-persisted allocations
  - Bridge plugin: veth pair, netns, IP assignment, routing
  - VXLAN overlay: VTEP creation, FDB entries, peer routes
- **feat:** Native container runtime via youki libcontainer
  - NativeRuntime: OCI container lifecycle without containerd/runc
  - Full OCI spec builder (rootfs, process, mounts, cgroups v2)
  - NativeImageService: image pulls via skopeo
  - Architecture: kubelet → libcontainer → kernel (no Go)
- **feat:** VM runtime for microVM-isolated pods
  - VmRuntime: each pod sandbox runs as a microVM with own kernel
  - Supports cloud-hypervisor (Rust-native), Firecracker, QEMU/KVM
  - Per-pod VM config via annotations (rustkube.io/vm-*)
  - virtiofs volume sharing, guest agent exec, SSH fallback
  - Runtime selection: `--runtime=native|vm|cri --vmm=auto|cloud-hypervisor|qemu|firecracker`

### 2026-03-17
- **chore:** Initial repository setup — Cargo workspace with 10 member crates
- **chore:** Scaffold rk-core, rk-store, rk-apiserver, rk-scheduler, rk-controllers, rk-kubelet, rk-proxy, rk-dns, rk-cni, rk-cloud
- **docs:** README, CLAUDE.md, CHANGELOG
