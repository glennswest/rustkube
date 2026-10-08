# Upstream Kubernetes feature inventory & gap-check

A checklist of what a **conformant drop-in Kubernetes** must provide, mapped to
this repo's v0.18.0 plus `turbomode` code as of 2026-09-29; the apiserver
reports the 1.36 API posture. Status: ✅ implemented surface · 🟡 partial ·
🔴 missing. A checkmark is not a conformance or deployment claim.

> The living parity backlog; conformance is the CNCF e2e `[Conformance]`
> suite. It has run against a synthetic control plane without kubelets
> (#67): 79/433 reported specs passed at 430b268, 75/428 at efbea2d before
> a missing-kubectl rerun. These are historical partial results, not certification.
> See [conformance.md](conformance.md) for exact runs and limitations.

## 1. API groups & resource kinds (apiserver)

| Group / kind | Must-have | Status | Where / note |
|---|---|---|---|
| `core/v1` — Pod, Service, Endpoints, Namespace, Node, ConfigMap, Secret, ServiceAccount, Event, PV, PVC | core | ✅ | `server.rs`, `discovery.rs` |
| `core/v1` — PodTemplate, ReplicationController, LimitRange, ResourceQuota | core | 🟡 | objects are served/discovered; LimitRanger enforces LimitRanges (#131); the ReplicationController controller and `/scale` (#125); ResourceQuota status and admission (#124) |
| `core/v1` — `pods/binding` | core | 🔴 | scheduler binds with a conditional PUT of `spec.nodeName` |
| `apps/v1` — Deployment, ReplicaSet, StatefulSet, DaemonSet | core | ✅ | apiserver + controllers |
| `apps/v1` — ControllerRevision; the `/scale` subresource | core | 🔴 | ControllerRevision is not in discovery and nothing writes one; `/scale` served for deployments, replicasets, statefulsets and CRDs with `subresources.scale` (#86) |
| `batch/v1` — Job, CronJob | core | ✅ | |
| `coordination.k8s.io/v1` — Lease | core | ✅ | leader election, node heartbeats |
| `rbac.authorization.k8s.io/v1` | core | 🟡 | `rbac_engine.rs`; no ClusterRole `aggregationRule`; escalation prevention (`bind`/`escalate` or held rules, `escalation.rs`, #98) |
| `apiextensions.k8s.io/v1` — CRD | core | 🟡 | served dynamically, keyed by group (#76), `/status` subresource; **no structural-schema validation, no conversion webhooks** |
| `autoscaling/v2` (+ `autoscaling/v1` view, #123) — HorizontalPodAutoscaler | core | 🟡 | served in both versions (v1 converted from the stored v2); the controller scales on cpu/memory from `metrics.k8s.io` (#89) |
| `apiregistration.k8s.io/v1` — APIService | core | 🔴 | 🟢 aggregated APIs proxied after authn/RBAC, availability checked, groups in discovery, front-proxy contract published; no upgrades (#83); aggregated discovery lists them as Stale (#107) |
| `admissionregistration.k8s.io/v1` — webhook configurations | core | 🟡 | mutating + validating webhooks called on every write (#82); no CEL `matchConditions`, AdmissionReview v1 only. Validating/MutatingAdmissionPolicy (+ bindings) served, not evaluated (#119, #234) |
| `networking.k8s.io/v1` — NetworkPolicy, Ingress, IngressClass, ServiceCIDR, IPAddress | core | 🟡 | API served; no Ingress controller. NetworkPolicy is enforced by Cilium. ServiceCIDR/IPAddress served with a bootstrapped `kubernetes` ServiceCIDR; allocation still uses `--service-cidr` and its own claim keys (#134) |
| `discovery.k8s.io/v1` — EndpointSlice | core | ✅ | served; the Service controller writes them (v0.7.5, #22) and mirrors selectorless Services' Endpoints into them (#133) |
| `pods/resize` — in-place pod resize (1.35 GA) | core | 🟡 | apiserver half (#136): subresource + upstream's resize validation; the kubelet does not resize yet (rustkube-node#192), so a running Pod's resize is refused |
| `policy/v1` — PodDisruptionBudget, Eviction | core | ✅ | `eviction.rs` (429 when blocked), `pdb.rs` (v0.7.18, #7). Not: 500 on multiple matching PDBs, `unhealthyPodEvictionPolicy`, `disruptedPods` |
| `storage.k8s.io/v1` — StorageClass, CSIDriver, CSINode, VolumeAttachment, CSIStorageCapacity, VolumeAttributesClass | core | ✅ | v0.7.11 (#24); see [storage.md](storage.md) |
| `scheduling.k8s.io/v1` — PriorityClass | core | ✅ | served, in `/apis`, and resolved at pod admission (#85) |
| `node.k8s.io/v1` — RuntimeClass | optional | 🟡 | served; RuntimeClass admission (missing class 403, overhead, scheduling merge) and overhead in the scheduler's fit (#135); whether the kubelet honours `handler` is rustkube-node's |
| `certificates.k8s.io/v1` — CSR | core | ✅ | with `/approval` and `/status`; `csr.rs` approves and signs |
| `events.k8s.io/v1` — Event | optional | ✅ | translated to/from stored core/v1 (v0.7.34, #48) |
| `flowcontrol.apiserver.k8s.io/v1` (APF) | optional | 🟡 | FlowSchema/PriorityLevelConfiguration served with `/status`, mandatory `exempt`/`catch-all` bootstrapped (#118); nothing classifies or queues requests, no `X-Kubernetes-PF-*` headers |
| `authentication.k8s.io/v1` — TokenReview; SelfSubjectReview | core | 🟡 | TokenReview served and in `/apis` (#85); no SelfSubjectReview (`kubectl auth whoami`) |
| `authorization.k8s.io/v1` — SelfSubjectAccessReview, SelfSubjectRulesReview, SubjectAccessReview, LocalSubjectAccessReview | core | ✅ | v0.9.0 (#59), v0.12.0 (#69) |
| `metrics.k8s.io` | optional | 🟡 | served by the apiserver from each node's cadvisor (#89); pod metrics need cadvisor#3 on stormcos |
| `project.openshift.io/v1` — Project, ProjectRequest | OpenShift | ✅ | Projects over Namespaces, owned by their requester, listed only to members (v0.15.0, #97) |

## 2. apiserver features

| Feature | Must-have | Status | Note |
|---|---|---|---|
| REST CRUD + `/status` | core | ✅ | a PUT to `/status` is conditional on the body's `resourceVersion` when supplied (#78); custom-resource main writes can still overwrite status (#128) |
| Watch (list+watch, chunked) + watch cache | core | ✅ | DELETED carries the last state, selectors apply to it (v0.15.2, #100) |
| Watch bookmarks, `sendInitialEvents` | core | ✅ | v0.7.25 (#39) |
| Label & field selectors, pagination | core | 🟡 | per-item RVs and pinned pages implemented; snapshot correctness needs fastetcd ≥ v1.6.1 (fastetcd#50, fixed); LIST/GET never served from the watch cache (#171); no automatic compaction (#139) |
| Server-Side Apply (`managedFields`, conflicts) | core | ✅ | v0.7.31–32 (#45) |
| Strategic-merge / JSON / merge patch | core | ✅ | strategic merge uses a fixed table of `patchMergeKey`s, not per-type schema; Service ports are wrong (#150) |
| protobuf wire codec | core | ✅ | both directions (v0.7.14) |
| TLS listener, serving-cert hot reload | core | 🟡 | mismatched pair refused (#93); client identity/trust do not reload (#105) |
| AuthN: x509 client cert, ServiceAccount/bearer JWT | core | 🟡 | rejected bearer can fall back to anonymous (#115); TokenRequest honours audiences, lifetime (unset: 1 h, as upstream, #206) and Pod/Secret/Node binding (#182) |
| AuthN: OIDC, webhook, bootstrap tokens | core | 🔴 | |
| AuthZ: RBAC | core | 🟡 | escalation prevention since #98; no `aggregationRule` controller |
| AuthZ: Node authorizer, webhook authorizer | core | 🔴 | `system:nodes` is bound to `cluster-admin` instead |
| Admission: webhooks | core | 🟡 | wired (#82): rules, selectors, failurePolicy, reinvocation, JSONPatch, warnings; CEL `matchConditions` not evaluated |
| Admission: built-ins | core | 🟡 | NamespaceLifecycle, ServiceAccount, DefaultTolerationSeconds, PodSecurity (subset), Priority, Service ClusterIP + NodePort allocation (type changes on update, #132), CronJob, PVC access-mode/expansion, ConfigMap/Secret key/immutability and sysctl validation; projected SA token volume; Pending phases and QoS; LimitRanger (#131, create only); ResourceQuota (#124, creates). DefaultStorageClass is applied by the PV controller instead |
| Aggregation layer | core | 🔴 | not wired (#83) |
| API Priority & Fairness, audit logging | optional | 🔴 | |
| Discovery, `/openapi/v2`, `/openapi/v3` | core | 🟡 | CRD schemas published per served version (#120, `kubectl explain` on CRs works); built-in types have GVK paths but **empty schemas**, so `kubectl explain` on them has nothing to show |

## 3. kube-controller-manager controllers

✅ present (`runner.rs`): Deployment, ReplicaSet, StatefulSet, DaemonSet, Job,
CronJob, Service (Endpoints + EndpointSlices), Namespace (default
ServiceAccount, deletion cascade), node lifecycle (with taint-based eviction),
PodDisruptionBudget status, garbage collector (background / foreground /
orphan), PersistentVolume binder, attach/detach, stormblock provisioner, CSR
approve + sign, PodMigration, VirtualMachine, VirtualMachineInstanceMigration
(control-plane half; the transfer is rustkube-node#40, #184), VMI launcher
Pods (#203). Leader election ✅.

🟡 placeholders: HPA (Resource metrics only; on stormcos pod metrics wait for cadvisor#3, #89); Gateway API (status
only, own classes only, `Programmed=False`, no address; #70, #91).

✅ the `kube-root-ca.crt` publisher (#67).

🔴 missing vs upstream: ServiceAccount token controller,
TTL-after-finished, NodeIPAM/route, ClusterRole aggregation.

With the turbomode implementation, all controller families use shared informer feeds and bounded
indexed object workers. GC and namespace finalization retain authoritative
absence reads before destructive cleanup. Successful-write overlays, UID/RV
preconditions and ambiguous-create expectations are implemented. The datastore
snapshot defect fastetcd#50 is fixed in v1.6.1 and dev acceptance (#146) is
complete; scale, multi-master and runtime acceptance remain #147/#149.

Drop-in acceptance (#2) remains unverified. The CLI accepts discrete TLS and
token flags but no kubeconfig; it has no shutdown signal handling. Default
ServiceAccount creation does not implement the missing token controller, and
the presence of a controller module does not demonstrate upstream parity.
[totrust#4](https://github.com/glennswest/totrust/issues/4) requires an
all-upstream baseline followed by a run with only the Rust controller-manager
substituted. On 2026-09-29, its
[PINS.yaml](https://github.com/glennswest/totrust/blob/main/PINS.yaml) specifies
Kubernetes v1.31.4, independently of this repository's 1.36 API posture.
Changing that shared pin belongs in totrust. The owner must identify an
isolated upstream test cluster and supported binary-deployment route before
acceptance runs. No such run has been completed here.

## 4. Scheduler (#3)

✅ filters: node Ready, unschedulable, taints, `nodeSelector`, required node
affinity, `nodeName`, inter-pod affinity/anti-affinity, topology spread,
resource fit (pod-level requests, #73), volume binding (PV node affinity,
`CSIStorageCapacity`, `ReadWriteOncePod`). ✅ scores: least requested, image
locality, node affinity, pod affinity, topology spread. ✅ priority sort.

✅ `schedulingGates` (`SchedulingGated`, add refused; #87).
🟡 preemption (#84: DefaultPreemption's victim choice, nomination, eviction through the Eviction API); upstream activeQ/backoffQ/unschedulable framework
parity and `nominatedNodeName`; scheduling profiles; NodePorts and BalancedAllocation;
upstream's score weights (scores are summed unweighted). With the turbomode implementation, a
serialized priority-ordered Pod/VMI queue and API retries are implemented,
with incremental accounting and retained bind/volume reservations. An unplaceable
Pod gets PodScheduled=False/Unschedulable (#194); `Scheduled` and
`FailedScheduling` Events are recorded (#138), the latter once per distinct
reason rather than upstream's aggregated count.

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
classes are handed to their CSI provisioner). The normal `stormblock` PVC
path is the kubelet's built-in blank-clone driver, not CSI or sbregistry clone
requests. ✅ expansion admission and preservation of the external-resizer
handshake (#63), tested with upstream sidecars; node filesystem growth is
external and not proved by that test. ✅ snapshot CRD/controller API
compatibility (#64), tested with the upstream snapshot-controller and a
simulated CSI sidecar; installation and actual snapshots remain external.
The built-in stormblock class has neither CSI snapshot sidecars nor a resizer.

## 8. OpenShift divergences (beyond CNCF)

- Scheduler profiles (LowNodeUtilization/HighNodeUtilization/NoScoring) via `config.openshift.io/v1 Scheduler` — see `scheduler-research.md`.
- Cluster `defaultNodeSelector` + namespace `openshift.io/node-selector` admission merge — #8 area.
- Descheduler operator (`KubeDescheduler`) — rebalancing, separate from scheduler.
- Multiarch Tuning Operator (`ClusterPodPlacementConfig`) — #8 (gates exist, #87).
- Projects are in scope and served (#97). SCC, Routes, OAuth, image streams — whether any is in scope is #70.

## Prioritized parity checklist

Done since the first version of this list: built-in admission (most), x509
authN, garbage collector, EndpointSlice, PriorityClass, PDB + Eviction, PV/PVC
binder + StorageClass, Server-Side Apply, watch bookmarks, CSR, OpenAPI v3
paths, `/status` optimistic concurrency (#78), Projects (#97).

**Open, conformance-blocking:**
1. ~~ResourceQuota~~ controller + admission (#124).
2. Node authorizer (nodes are `cluster-admin` today).
3. ~~`/scale` (#86)~~ served.
4. Kubelet exec/attach/port-forward (rustkube-node#56).
5. ~~Scheduler: preemption~~ (#84, 2026-10-07).
6. Live turbomode acceptance at scale and under failover (#147/#149); datastore
   snapshot correctness (fastetcd#50) is fixed in fastetcd v1.6.1.
   The protobuf empty-UID GC defect (#99) was fixed in v0.15.3.

**Then:**
7. Pod metrics from cadvisor on stormcos (cadvisor#3) → HPA scales there (#89).
8. OpenAPI schemas (`kubectl explain`), CRD schema validation, conversion webhooks.
9. Validate the indexed informer implementation at scale and under failover (#66/#147/#149).
10. API Priority & Fairness, audit, ValidatingAdmissionPolicy.
11. Ingress/Gateway data plane, Routes (#70, #91), LoadBalancer.
12. Multi-arch admission (#8), OpenShift extras.
