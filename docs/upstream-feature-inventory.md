# Upstream Kubernetes feature inventory & gap-check

A complete-as-practical checklist of what a **conformant drop-in Kubernetes**
(K8s 1.32) must provide, mapped to our current implementation. Status:
✅ implemented · 🟡 partial · 🔴 missing. "Where" is the repo/crate.

> Curated from the upstream component surface (not from the deep-research
> harness, which verifies discrete claims rather than enumerations). Treat as
> the living parity backlog; conformance = the CNCF e2e `[Conformance]` suite.

## 1. API groups & resource kinds (apiserver)

| Group / kind | Must-have | Status | Where / note |
|---|---|---|---|
| `core/v1` — Pod, Service, Endpoints, Namespace, Node, ConfigMap, Secret, ServiceAccount, Event, PV, PVC, ReplicationController, LimitRange, ResourceQuota, Binding | core | 🟡 | apiserver — most CRUD present; **Binding subresource, ResourceQuota, LimitRange objects** 🔴 |
| `apps/v1` — Deployment, ReplicaSet, StatefulSet, DaemonSet, ControllerRevision | core | ✅ | apiserver + controllers |
| `batch/v1` — Job, CronJob | core | ✅ | apiserver + controllers |
| `coordination.k8s.io/v1` — Lease | core | ✅ | used by leader election |
| `rbac.authorization.k8s.io/v1` — Role/Binding, ClusterRole/Binding | core | ✅ | apiserver + rbac_engine |
| `apiextensions.k8s.io/v1` — CustomResourceDefinition | core | 🟡 | CRD registry; **structural schema validation + conversion webhooks** 🔴 |
| `autoscaling/v2` — HorizontalPodAutoscaler | core | 🟡 | HPA controller; v2 metrics API 🟡 |
| `apiregistration.k8s.io/v1` — APIService (aggregation) | core | 🟡 | aggregation.rs (registry + proxy) |
| `admissionregistration.k8s.io/v1` — Mutating/ValidatingWebhookConfiguration | core | 🟡 | webhook configs parsed; **ValidatingAdmissionPolicy (CEL)** 🔴 |
| `networking.k8s.io/v1` — NetworkPolicy, Ingress, IngressClass | core | 🟡 | kinds in discovery; **API routes + Ingress controller** 🔴 (NetworkPolicy enforcement exists node-side) |
| `discovery.k8s.io/v1` — EndpointSlice | core | 🔴 | only legacy Endpoints today |
| `policy/v1` — PodDisruptionBudget | core | 🔴 | #7 |
| `storage.k8s.io/v1` — StorageClass, CSINode, CSIDriver, VolumeAttachment | core | 🔴 | no dynamic provisioning |
| `scheduling.k8s.io/v1` — PriorityClass | core | 🔴 | preemption reads priority but no PriorityClass object |
| `node.k8s.io/v1` — RuntimeClass | optional | 🔴 | |
| `certificates.k8s.io/v1` — CertificateSigningRequest | core | 🔴 | needed for kubelet TLS bootstrap |
| `events.k8s.io/v1` — Event | optional | 🟡 | core/v1 events only |
| `flowcontrol.apiserver.k8s.io/v1` — FlowSchema, PriorityLevelConfiguration (APF) | optional | 🔴 | no API Priority & Fairness |
| `authentication.k8s.io/v1` — TokenReview, SubjectAccessReview (`authorization.k8s.io`) | core | 🔴 | needed for webhook authn/authz + `kubectl auth can-i` |

## 2. apiserver features

| Feature | Must-have | Status | Note |
|---|---|---|---|
| REST CRUD + status subresources | core | ✅ | |
| Watch (list+watch, chunked) | core | ✅ | + **watch cache** ✅ (#5) |
| Watch **bookmarks** | core | 🔴 | |
| Label & field selectors | core | ✅ | |
| Pagination (limit/continue) | core | ✅ | |
| **Server-Side Apply** (`fieldManager`, managedFields) | core | 🔴 | |
| Strategic-merge / JSON / merge patch | core | 🟡 | JSON-patch present |
| TLS listener (HTTPS) | core | ✅ | (#4) |
| **AuthN**: serviceaccount JWT, bearer | core | 🟡 | ✅ SA tokens/bearer; 🔴 **x509 client-cert**, OIDC, webhook, bootstrap-token |
| **AuthZ**: RBAC | core | 🟡 | ✅ RBAC; 🔴 **Node authorizer**, Webhook authorizer |
| Admission: **webhooks** (mutating/validating) | core | ✅ | config-driven chains |
| Admission: **built-in plugins** (NamespaceLifecycle, ServiceAccount, LimitRanger, ResourceQuota, PodSecurity, DefaultStorageClass, DefaultTolerationSeconds, Mutating/Validating) | core | 🔴 | none of the built-ins yet |
| Aggregation layer (proxy to extension apiservers) | core | 🟡 | registry + proxy |
| API Priority & Fairness | optional | 🔴 | |
| Audit logging | optional | 🔴 | |
| OpenAPI/discovery (`/api`,`/apis`,`/openapi/v3`) | core | 🟡 | discovery ✅; OpenAPI schema 🔴 |

## 3. kube-controller-manager controllers

✅ present: Deployment, ReplicaSet, StatefulSet, DaemonSet, Job, CronJob, Service (Endpoints), Namespace, Node-lifecycle, HPA, Gateway, (Migration – ours). **Leader election ✅ (#2).**

🔴 missing vs upstream: **garbage collector (owner-ref/finalizers)**, **ResourceQuota**, **ServiceAccount + token/root-CA-cert**, **EndpointSlice** controller, **PersistentVolume binder + provisioner**, **PodDisruptionBudget/disruption** controller (#7), **CertificateSigning** approver, **TTL-after-finished**, **NodeIPAM/route**, **ClusterRole aggregation**, endpoint-slice mirroring, taint-eviction (node-not-ready → pod eviction).

## 4. Scheduler (#3)

🟡 filter/score plugins + preemption exist. Parity backlog (see `scheduler-research.md`): full default plugin set with exact weights, PodTopologySpread, InterPodAffinity, VolumeBinding, the activeQ/backoffQ/unschedulable queue, PriorityClass, scheduling profiles, **PodSchedulingGates** (needed for multi-arch #8).

## 5. Node components (rustkube-node — greenfield)

🔴 **kubelet** (registration, CRI, pod lifecycle, probes, cgroups v2, evictions/OOM, device plugins, topology manager, static pods, node status/leases), 🔴 **kube-proxy** (iptables/ipvs/nftables), 🔴 **CNI** (pod networking). Tracked: rustkube-node #1/#2/#3.

## 6. Networking

✅ Services (ClusterIP/NodePort — proxy), 🟡 headless/ExternalName, 🔴 **EndpointSlice**, 🔴 LoadBalancer (needs cloud provider), 🟡 NetworkPolicy (node enforcement, no API), 🔴 Ingress controller, 🟡 Gateway API (controller), external DNS ✅ (microdns #3).

## 7. Storage

🟡 PV/PVC objects, 🔴 dynamic provisioning, 🔴 StorageClass, 🔴 CSI (attach/mount/provision), 🔴 VolumeSnapshots. CSI client traits exist node-side.

## 8. OpenShift divergences (beyond CNCF)

- Scheduler profiles (LowNodeUtilization/HighNodeUtilization/NoScoring) via `config.openshift.io/v1 Scheduler` — see `scheduler-research.md`.
- Cluster `defaultNodeSelector` + namespace `openshift.io/node-selector` **admission merge** (open question) — #8 area.
- **Descheduler operator** (`KubeDescheduler` singleton `cluster`, profiles, `deschedulingIntervalSeconds=3600`) — rebalancing, separate from scheduler.
- **Multiarch Tuning Operator** (`ClusterPodPlacementConfig` singleton `cluster`) — #8.
- Projects (Namespace+), SCC (PodSecurity++), Routes (Ingress++), oauth, image streams — large surface, optional for K8s conformance.

## Prioritized parity checklist (next)

**P0 — conformance-blocking core:**
1. Built-in admission: NamespaceLifecycle, ServiceAccount, DefaultTolerationSeconds, PodSecurity, LimitRanger, ResourceQuota.
2. x509 client-cert authN + Node authorizer (kubelet/components auth).
3. Garbage collector (owner refs + finalizers) + ServiceAccount controller (default SA + token).
4. EndpointSlice (API + controller) + Service completeness.
5. Node level: kubelet joins + runs pods (rustkube-node #1) + kube-proxy (#2) + CNI (#3).
6. Scheduler core parity (#3).
7. PriorityClass object + scheduling.k8s.io.

**P1 — important:**
8. PodDisruptionBudget + Eviction API + drain (#7).
9. PV/PVC binder + StorageClass + CSI provisioning.
10. Server-Side Apply + watch bookmarks.
11. CSR/certificates (kubelet TLS bootstrap).
12. Multi-arch admission (#8) incl. PodSchedulingGates.

**P2 — scale/advanced:**
13. API Priority & Fairness, audit, OpenAPI v3.
14. ValidatingAdmissionPolicy (CEL), conversion webhooks.
15. Ingress/Gateway, NetworkPolicy API, LoadBalancer/cloud-controller.
16. OpenShift extras (descheduler, scheduler profiles, node-selector admission).
