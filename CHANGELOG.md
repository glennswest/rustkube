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

### 2026-03-17
- **chore:** Initial repository setup — Cargo workspace with 10 member crates
- **chore:** Scaffold rk-core, rk-store, rk-apiserver, rk-scheduler, rk-controllers, rk-kubelet, rk-proxy, rk-dns, rk-cni, rk-cloud
- **docs:** README, CLAUDE.md, CHANGELOG
