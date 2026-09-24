# Upstream Kubernetes feature inventory & gap-check

A checklist of what a **conformant drop-in Kubernetes** must provide, mapped to
this repo's code as of v0.14.1 (2026-09-24; the apiserver reports the 1.36 API
posture). Status: ✅ implemented · 🟡 partial · 🔴 missing.

> The living parity backlog; conformance is the CNCF e2e `[Conformance]`
> suite, which has never been run here (#67). Rewritten from the code on
> 2026-09-24 (#80) — the first version, from 2026-07-15, had not been updated
> since and about twenty statuses were wrong.

## 1. API groups & resource kinds (apiserver)

| Group / kind | Must-have | Status | Where / note |
|---|---|---|---|
| `core/v1` — Pod, Service, Endpoints, Namespace, Node, ConfigMap, Secret, ServiceAccount, Event, PV, PVC | core | ✅ | `server.rs`, `discovery.rs` |
| `core/v1` — ReplicationController, LimitRange, ResourceQuota, the `pods/binding` subresource | core | 🔴 | not in discovery; a stored ResourceQuota is a blob nothing enforces. The scheduler binds with a PUT of `spec.nodeName` |
| `apps/v1` — Deployment, ReplicaSet, StatefulSet, DaemonSet | core | ✅ | apiserver + controllers |
| `apps/v1` — ControllerRevision; the `/scale` subresource | core | 🔴 | ControllerRevision is not in discovery and nothing writes one; `deployments/scale` is advertised with no route (#86) |
| `batch/v1` — Job, CronJob | core | ✅ | |
| `coordination.k8s.io/v1` — Lease | core | ✅ | leader election, node heartbeats |
| `rbac.authorization.k8s.io/v1` | core | ✅ | `rbac_engine.rs`; no ClusterRole `aggregationRule` |
| `apiextensions.k8s.io/v1` — CRD | core | 🟡 | served dynamically, keyed by group (#76), `/status` subresource; **no structural-schema validation, no conversion webhooks** |
| `autoscaling/v2` — HorizontalPodAutoscaler | core | 🟡 | object served; the controller is a placeholder (#89) |
| `apiregistration.k8s.io/v1` — APIService | core | 🔴 | objects stored; `aggregation.rs` is not wired in, nothing is proxied (#83) |
| `admissionregistration.k8s.io/v1` — webhook configurations | core | 🔴 | objects stored; `admission.rs` is not wired in, no webhook is called (#82). ValidatingAdmissionPolicy absent |
| `networking.k8s.io/v1` — NetworkPolicy, Ingress, IngressClass | core | 🟡 | API served; no Ingress controller. NetworkPolicy is enforced by Cilium |
| `discovery.k8s.io/v1` — EndpointSlice | core | ✅ | served; the Service controller writes them (v0.7.5, #22) |
| `policy/v1` — PodDisruptionBudget, Eviction | core | ✅ | `eviction.rs` (429 when blocked), `pdb.rs` (v0.7.18, #7). Not: 500 on multiple matching PDBs, `unhealthyPodEvictionPolicy`, `disruptedPods` |
| `storage.k8s.io/v1` — StorageClass, CSIDriver, CSINode, VolumeAttachment, CSIStorageCapacity | core | ✅ | v0.7.11 (#24); see [storage.md](storage.md) |
| `scheduling.k8s.io/v1` — PriorityClass | core | 🟡 | served and resolved at pod admission, **not in `/apis`** (#85) |
| `node.k8s.io/v1` — RuntimeClass | optional | 🔴 | |
| `certificates.k8s.io/v1` — CSR | core | ✅ | with `/approval` and `/status`; `csr.rs` approves and signs |
| `events.k8s.io/v1` — Event | optional | ✅ | translated to/from stored core/v1 (v0.7.34, #48) |
| `flowcontrol.apiserver.k8s.io/v1` (APF) | optional | 🔴 | |
| `authentication.k8s.io/v1` — TokenReview; SelfSubjectReview | core | 🟡 | TokenReview served, **not in `/apis`** (#85); no SelfSubjectReview (`kubectl auth whoami`) |
| `authorization.k8s.io/v1` — SelfSubjectAccessReview, SelfSubjectRulesReview, SubjectAccessReview, LocalSubjectAccessReview | core | ✅ | v0.9.0 (#59), v0.12.0 (#69) |
| `metrics.k8s.io` | optional | 🔴 | needs aggregation (#83) and a metrics server |

## 2. apiserver features

| Feature | Must-have | Status | Note |
|---|---|---|---|
| REST CRUD + `/status` | core | ✅ | a PUT to `/status` ignores the body's `resourceVersion` (#78) |
| Watch (list+watch, chunked) + watch cache | core | ✅ | |
| Watch bookmarks, `sendInitialEvents` | core | ✅ | v0.7.25 (#39) |
| Label & field selectors, pagination | core | ✅ | |
| Server-Side Apply (`managedFields`, conflicts) | core | ✅ | v0.7.31–32 (#45) |
| Strategic-merge / JSON / merge patch | core | ✅ | strategic merge uses a fixed table of `patchMergeKey`s, not per-type schema |
| protobuf wire codec | core | ✅ | both directions (v0.7.14) |
| TLS listener, serving-cert hot reload | core | ✅ | |
| AuthN: x509 client cert, ServiceAccount/bearer JWT | core | ✅ | |
| AuthN: OIDC, webhook, bootstrap tokens | core | 🔴 | |
| AuthZ: RBAC | core | ✅ | |
| AuthZ: Node authorizer, webhook authorizer | core | 🔴 | `system:nodes` is bound to `cluster-admin` instead |
| Admission: webhooks | core | 🔴 | not wired (#82) |
| Admission: built-ins | core | 🟡 | NamespaceLifecycle, ServiceAccount, DefaultTolerationSeconds, PodSecurity (subset), Priority, Service IP allocation, CronJob and PVC access-mode validation. 🔴 LimitRanger, ResourceQuota; DefaultStorageClass is applied by the PV controller instead |
| Aggregation layer | core | 🔴 | not wired (#83) |
| API Priority & Fairness, audit logging | optional | 🔴 | |
| Discovery, `/openapi/v2`, `/openapi/v3` | core | 🟡 | served with GVK paths but **empty schemas**, so `kubectl explain` has nothing to show |

## 3. kube-controller-manager controllers

✅ present (`runner.rs`): Deployment, ReplicaSet, StatefulSet, DaemonSet, Job,
CronJob, Service (Endpoints + EndpointSlices), Namespace (default
ServiceAccount, deletion cascade), node lifecycle (with taint-based eviction),
PodDisruptionBudget status, garbage collector (background / foreground /
orphan), PersistentVolume binder, attach/detach, stormblock provisioner, CSR
approve + sign, PodMigration, VirtualMachine. Leader election ✅.

🟡 placeholders: HPA (no metrics, cannot scale down, #89); Gateway API (status
only, hardcoded address, #91).

🔴 missing vs upstream: ResourceQuota, ServiceAccount token and
`kube-root-ca.crt` publisher, TTL-after-finished, NodeIPAM/route, ClusterRole
aggregation, endpoint-slice mirroring, ReplicationController.

All of them poll-and-list on a fixed interval; none uses an informer (#66).

## 4. Scheduler (#3)

✅ filters: node Ready, unschedulable, taints, `nodeSelector`, required node
affinity, `nodeName`, inter-pod affinity/anti-affinity, topology spread,
resource fit (pod-level requests, #73), volume binding (PV node affinity,
`CSIStorageCapacity`, `ReadWriteOncePod`). ✅ scores: least requested, image
locality, node affinity, pod affinity, topology spread. ✅ priority sort.

🔴 preemption (`preemption.rs` is never called, #84); `schedulingGates` (gated
pods are scheduled, #87); the activeQ/backoffQ/unschedulable queue and
`nominatedNodeName`; scheduling profiles; NodePorts and BalancedAllocation;
upstream's score weights (scores are summed unweighted).

## 5. Node components

Not in this repo: the kubelet is [rustkube-node](https://github.com/glennswest/rustkube-node);
kube-proxy is replaced by Cilium. What this apiserver needs from the kubelet —
`containerLogs` (served) and `exec`/`attach`/`portForward` (**not served**,
rustkube-node#56) — is in the README.

## 6. Networking

✅ Services (ClusterIP allocation from `--service-cidr`; the data plane is
Cilium), ✅ EndpointSlice, ✅ NetworkPolicy API (enforced by Cilium), 🔴
LoadBalancer (`pkg/cloud` is empty), 🔴 Ingress controller, 🟡 Gateway API
(status only, #91), 🔴 Routes (stored, nothing routes, #70).

## 7. Storage

✅ PV/PVC binding, phases, protection, reclaim; ✅ StorageClass, CSIDriver,
VolumeAttachment; 🟡 dynamic provisioning (the `stormblock` class only; other
classes are handed to their CSI provisioner); 🔴 volume expansion (#63); 🔴
VolumeSnapshots (#64).

## 8. OpenShift divergences (beyond CNCF)

- Scheduler profiles (LowNodeUtilization/HighNodeUtilization/NoScoring) via `config.openshift.io/v1 Scheduler` — see `scheduler-research.md`.
- Cluster `defaultNodeSelector` + namespace `openshift.io/node-selector` admission merge — #8 area.
- Descheduler operator (`KubeDescheduler`) — rebalancing, separate from scheduler.
- Multiarch Tuning Operator (`ClusterPodPlacementConfig`) — #8, needs #87.
- Projects, SCC, Routes, OAuth, image streams — whether any is in scope is #70.

## Prioritized parity checklist

Done since the first version of this list: built-in admission (most), x509
authN, garbage collector, EndpointSlice, PriorityClass, PDB + Eviction, PV/PVC
binder + StorageClass, Server-Side Apply, watch bookmarks, CSR, OpenAPI v3
paths.

**Open, conformance-blocking:**
1. Admission webhooks (#82), LimitRanger, ResourceQuota.
2. Node authorizer (nodes are `cluster-admin` today).
3. Discovery for PriorityClass/TokenReview (#85); `/scale` (#86).
4. Kubelet exec/attach/port-forward (rustkube-node#56).
5. Scheduler: preemption (#84), scheduling gates (#87), queue.
6. `/status` optimistic concurrency (#78).

**Then:**
7. Aggregation (#83) → metrics API → a real HPA (#89).
8. OpenAPI schemas (`kubectl explain`), CRD schema validation, conversion webhooks.
9. Informer-based controllers, if scale measurements say so (#66).
10. API Priority & Fairness, audit, ValidatingAdmissionPolicy.
11. Ingress/Gateway data plane, Routes (#70, #91), LoadBalancer.
12. Multi-arch admission (#8), OpenShift extras.
