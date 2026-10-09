# Upstream Kubernetes feature inventory & gap-check

A checklist of what a **conformant drop-in Kubernetes** must provide, mapped to
this repo's main (v0.18.0 plus unreleased work) as of 2026-10-09; the apiserver
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
| `core/v1` — PodTemplate, ReplicationController, LimitRange, ResourceQuota | core | 🟡 | served and discovered; LimitRanger admission on create (#131); ReplicationController controller + `/scale` (#125); ResourceQuota controller + create admission, resize/expansion not charged (#124, #242) |
| `core/v1` — `pods/binding` | core | 🔴 | scheduler binds with a conditional PUT of `spec.nodeName` |
| `apps/v1` — Deployment, ReplicaSet, StatefulSet, DaemonSet | core | ✅ | apiserver + controllers; StatefulSet has no RollingUpdate (#255) |
| `apps/v1` — ControllerRevision; the `/scale` subresource | core | 🟡 | ControllerRevision is not in discovery and nothing writes one; `/scale` served for deployments, replicasets, statefulsets, replicationcontrollers and CRDs with `subresources.scale` (#86, #125) |
| `batch/v1` — Job, CronJob | core | ✅ | |
| `coordination.k8s.io/v1` — Lease | core | ✅ | leader election, node heartbeats |
| `rbac.authorization.k8s.io/v1` | core | 🟡 | `rbac_engine.rs`; no ClusterRole `aggregationRule`; escalation prevention (`bind`/`escalate` or held rules, `escalation.rs`, #98) |
| `apiextensions.k8s.io/v1` — CRD | core | 🟡 | served dynamically, keyed by group (#76), `/status` and `/scale` subresources, `metadata.generation` (#198); structural schema defaulting, pruning and fieldValidation (#121); **no type/format/enum/required validation (#240), no conversion webhooks** |
| `autoscaling/v2` (+ `autoscaling/v1` view, #123) — HorizontalPodAutoscaler | core | 🟡 | served in both versions (v1 converted from the stored v2); the controller scales on cpu/memory from `metrics.k8s.io` (#89) |
| `apiregistration.k8s.io/v1` — APIService | core | 🟡 | aggregated APIs proxied after authn/RBAC, availability checked, groups in discovery, front-proxy contract published; no upgrades (#83); aggregated discovery lists them as Stale (#107) |
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
| `authentication.k8s.io/v1` — TokenReview; SelfSubjectReview | core | 🟡 | TokenReview served and in `/apis` (#85); SelfSubjectReview (`kubectl auth whoami`, #116); the authenticated identity carries no uid/extra (#251) |
| `authorization.k8s.io/v1` — SelfSubjectAccessReview, SelfSubjectRulesReview, SubjectAccessReview, LocalSubjectAccessReview | core | ✅ | v0.9.0 (#59), v0.12.0 (#69); a Table-only Accept is 406 (#126) |
| `resource.k8s.io/v1` (DRA) — DeviceClass, ResourceClaim, ResourceClaimTemplate, ResourceSlice | optional | 🟡 | served, CRUD only (#137); no allocation, no claim-from-template controller (#225) |
| `metrics.k8s.io` | optional | 🟡 | served by the apiserver from each node's cadvisor (#89); pod metrics need cadvisor#3 on stormcos |
| `project.openshift.io/v1` — Project, ProjectRequest | OpenShift | ✅ | Projects over Namespaces, owned by their requester, listed only to members (v0.15.0, #97) |
| `authorization.openshift.io/v1` — (Local)SubjectAccessReview, (Local)ResourceAccessReview | OpenShift | ✅ | `oc policy who-can`, `oc adm new-project` (#106) |

## 2. apiserver features

| Feature | Must-have | Status | Note |
|---|---|---|---|
| REST CRUD + `/status` | core | ✅ | a PUT to `/status` is conditional on the body's `resourceVersion` when supplied (#78); a CR with the status subresource keeps status out of main writes (#128); CR `/status` is served even without the subresource (#243) |
| Watch (list+watch, chunked) + watch cache | core | ✅ | DELETED carries the last state, selectors apply to it (v0.15.2, #100); `timeoutSeconds` ends a watch (#165), no default randomized deadline (#246) |
| Watch bookmarks, `sendInitialEvents` | core | ✅ | v0.7.25 (#39) |
| Label & field selectors, pagination | core | ✅ | per-item RVs and pinned pages; snapshot correctness needs fastetcd ≥ v1.6.1 (fastetcd#50, fixed); LIST/GET with `resourceVersion` 0/N served from the watch cache (#171); compaction every `--etcd-compaction-interval`, a compacted continue token is 410 Expired (#139) |
| Server-Side Apply (`managedFields`, conflicts) | core | ✅ | v0.7.31–32 (#45) |
| Strategic-merge / JSON / merge patch | core | ✅ | strategic merge uses a fixed table of `patchMergeKey`s, not per-type schema; Service ports merge by port + protocol (#150) |
| protobuf wire codec; YAML bodies | core | ✅ | protobuf both directions (v0.7.14); YAML request bodies (#122) |
| `fieldValidation` | core | 🟡 | Strict/Warn/Ignore on built-in POST/PUT (#122) and CRs (#121); unknown fields kept under Warn/Ignore, PATCH/apply unchecked (#241) |
| TLS listener, serving-cert hot reload | core | ✅ | mismatched pair refused (#93); client certificates and the client CA reload too (#105) |
| AuthN: x509 client cert, ServiceAccount/bearer JWT | core | 🟡 | a rejected bearer token is 401 (#115); `--token-auth-file` static tokens (#188); several `--service-account-key-file` keys for rotation (#223); TokenRequest honours audiences, lifetime (unset: 1 h, as upstream, #206) and Pod/Secret/Node binding (#182) |
| AuthN: OIDC, webhook, bootstrap tokens | core | 🔴 | (a static token file is supported, #188) |
| AuthZ: RBAC | core | 🟡 | escalation prevention since #98; least-privilege controller-manager and scheduler roles (#176); no `aggregationRule` controller; Namespace writes authorized cluster-scoped (#249) |
| AuthZ: Node authorizer, webhook authorizer | core | 🔴 | `system:nodes` is bound to `cluster-admin` instead (#228) |
| Admission: webhooks | core | 🟡 | wired (#82): rules, selectors, failurePolicy, reinvocation, JSONPatch, warnings; CEL `matchConditions` not evaluated (#217); admission policies served, not evaluated (#234) |
| Admission: built-ins | core | 🟡 | NamespaceLifecycle, ServiceAccount, DefaultTolerationSeconds, PodSecurity (subset), Priority, Service ClusterIP + NodePort allocation (type changes on update, #132), CronJob, PVC access-mode/expansion, ConfigMap/Secret key/immutability and sysctl validation, Secret `stringData` folded into `data` (#101); projected SA token volume; Pending phases and QoS; RuntimeClass (#135); scheduling gates may not be added (#87); LimitRanger (#131, create only); ResourceQuota (#124, creates); `storage.storm.io` requester stamp (#210). DefaultStorageClass is applied by the PV controller instead |
| Aggregation layer | core | 🟡 | wired (#83): proxy after authn/RBAC, availability, `extension-apiserver-authentication`; upgrades refused (501) |
| API Priority & Fairness, audit logging | optional | 🔴 | APF objects served, not enforced (#118, #248); no audit log |
| Discovery, `/openapi/v2`, `/openapi/v3` | core | 🟡 | aggregated discovery (`apidiscovery.k8s.io/v2`) on `/api` and `/apis` (#107); a resource no built-in group-version serves is 404 (#110); CRD schemas published per served version (#120, `kubectl explain` on CRs works); built-in types have GVK paths but **empty schemas**, so `kubectl explain` on them has nothing to show |

## 3. kube-controller-manager controllers

✅ present (`runner.rs`): Deployment, ReplicaSet, ReplicationController
(#125), StatefulSet, DaemonSet, Job, CronJob, Service (Endpoints +
EndpointSlices; selectorless Services' Endpoints mirrored, #133), Namespace
(default ServiceAccount, deletion cascade), node lifecycle (with taint-based
eviction), PodDisruptionBudget status, ResourceQuota status (#124),
ephemeral volumes (#94), garbage collector (background / foreground /
orphan), PersistentVolume binder, attach/detach, stormblock provisioner, CSR
approve + sign (`kubernetes.io/*` signers; external signers left alone,
#199), PodMigration, VirtualMachine, VirtualMachineInstanceMigration
(control-plane half; the transfer is rustkube-node#40, #184), VMI launcher
Pods (#203) — the KubeVirt ones only while their CRDs are Established
(#172). Leader election ✅.

🟡 partial: HPA (upstream's calculator and behavior on `Resource` metrics
from `metrics.k8s.io`; no Pods/Object/External metrics; targets Deployments,
ReplicaSets and StatefulSets only, by writing `spec.replicas`, not `/scale`; on stormcos pod
metrics wait for cadvisor#3, #89); Gateway API (status only, own classes
only, `Programmed=False`, no address; #70, #91).

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
✅ pod count (`allocatable.pods`, #194). ✅ preemption (#84: DefaultPreemption's
victim choice, PDB-aware, `nominatedNodeName`, eviction through the Eviction API).
🔴 upstream activeQ/backoffQ/unschedulable framework parity; scheduling
profiles; NodePorts and BalancedAllocation; upstream's score weights (scores
are summed unweighted); a NotReady node refuses every Pod whatever its
tolerations (#250). With the turbomode implementation, a
serialized priority-ordered Pod/VMI queue and API retries are implemented,
with incremental accounting and retained bind/volume reservations. An unplaceable
Pod gets PodScheduled=False/Unschedulable (#194); `Scheduled` and
`FailedScheduling` Events are recorded (#138), the latter once per distinct
reason rather than upstream's aggregated count.

## 5. Node components

Not in this repo: the kubelet is [rustkube-node](https://github.com/glennswest/rustkube-node);
kube-proxy is replaced by Cilium. What this apiserver needs from the kubelet —
`containerLogs` (served), `exec`/`attach`/`portForward` (**not served**,
rustkube-node#56), `/logs/` behind `nodes/{name}/proxy` (not served,
rustkube-node#198), in-place resize (rustkube-node#192) and the VMI verbs
(`/vmVerb`, rustkube-node#94) — is in the README.

## 6. Networking

✅ Services (ClusterIP from `--service-cidr`, NodePorts from
`--service-node-port-range`, both following type changes, #132; no
`healthCheckNodePort`, #244; the data plane is Cilium), 🟡 ServiceCIDR /
IPAddress (served, not used for allocation, #245), ✅ EndpointSlice, ✅ NetworkPolicy API (enforced by Cilium), 🔴
LoadBalancer (`pkg/cloud` is empty), 🔴 Ingress controller, 🟡 Gateway API
(status only, #91), 🔴 Routes (stored, nothing routes, #70).

## 7. Storage

✅ PV/PVC binding, phases, protection, reclaim; ✅ StorageClass, CSIDriver,
VolumeAttachment; 🟡 dynamic provisioning (the `stormblock` class only; other
classes are handed to their CSI provisioner; the stormblock provisioner
checks the class's provisioner, #92, and copies the claim's `volumeMode`,
#201). ✅ generic ephemeral volumes (#94). The normal `stormblock` PVC
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
paths, `/status` optimistic concurrency (#78), Projects (#97), admission
webhooks (#82), RBAC escalation prevention (#98), ResourceQuota (#124),
LimitRanger (#131), `/scale` (#86), preemption (#84), scheduling gates (#87),
aggregation (#83). Several of these wait for their rig run on a test machine
before their issue closes.

**Open, conformance-blocking:**
1. Node authorizer (nodes are `cluster-admin` today, #228).
2. Kubelet exec/attach/port-forward (rustkube-node#56); graceful pod deletion (#224).
3. Live turbomode acceptance at scale and under failover (#147/#149); datastore
   snapshot correctness (fastetcd#50) is fixed in fastetcd v1.6.1.
   The protobuf empty-UID GC defect (#99) was fixed in v0.15.3.

**Then:**
4. Pod metrics from cadvisor on stormcos (cadvisor#3) → HPA scales there (#89).
5. Built-in OpenAPI schemas (`kubectl explain`), CR value validation (#240), conversion webhooks.
6. Validate the indexed informer implementation at scale and under failover (#66/#147/#149).
7. API Priority & Fairness enforcement (#248), audit, admission policy evaluation (#234).
8. Ingress/Gateway data plane, Routes (#70, #91), LoadBalancer.
9. DRA allocation (#225); multi-arch admission (#8), OpenShift extras.
