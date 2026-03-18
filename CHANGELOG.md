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

### 2026-03-17
- **chore:** Initial repository setup — Cargo workspace with 10 member crates
- **chore:** Scaffold rk-core, rk-store, rk-apiserver, rk-scheduler, rk-controllers, rk-kubelet, rk-proxy, rk-dns, rk-cni, rk-cloud
- **docs:** README, CLAUDE.md, CHANGELOG
