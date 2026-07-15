# Draining, eviction & multi-arch — verified research (for #7, #8)

Adversarially-verified (3-0) against primary sources (kubernetes.io, OpenShift
docs + GitHub). Targets K8s 1.32 / OpenShift 4.x.

## Node draining & eviction (#7)

**Eviction API** — `POST` an `Eviction` object (`kind: Eviction`, `policy/v1`,
metadata name/namespace) to `/api/v1/namespaces/{ns}/pods/{name}/eviction`. It
behaves as a **policy-controlled DELETE** that respects PDBs and
`terminationGracePeriodSeconds`. Returns:
- **200 OK** — allowed, pod deleted (and **always 200 if the pod is covered by no PDB**).
- **429 Too Many Requests** — blocked by a PDB (or API rate limiting).
- **500** — misconfiguration (e.g. multiple PDBs match the same pod).

`kubectl drain` calls the Eviction API (never a direct delete), **retrying 429s**
until all pods on the node terminate or `--timeout` elapses.

**PodDisruptionBudget** (`policy/v1`, GA 1.21):
- Spec: mutually-exclusive `minAvailable` / `maxUnavailable` (each IntOrString =
  count or %) + a label `selector`. An eviction is allowed if ≥ `minAvailable`
  pods remain available (or ≤ `maxUnavailable` are down).
- Status the disruption controller computes: `disruptionsAllowed`,
  `currentHealthy`, `desiredHealthy`, `expectedPods` (ints), and a
  `disruptedPods` map (pod → time the apiserver processed the eviction, before
  the controller observes it). `expectedPods`/`desiredHealthy` derive from the
  workload `.spec.replicas` found via the pods' `ownerReferences`.
- `.spec.unhealthyPodEvictionPolicy` (1.26+): **IfHealthyBudget** (default —
  unhealthy/not-ready pods evictable only when `currentHealthy >= desiredHealthy`)
  or **AlwaysAllow** (unhealthy pods always evictable).

**Implementation:** apiserver serves the Eviction subresource + PDB resource; a
disruption controller keeps PDB status current; a drain helper cordons
(`node.spec.unschedulable=true`, honored by the scheduler `NodeUnschedulable`
filter) then evicts, skipping DaemonSet + static/mirror pods.

## Multi-arch scheduling (#8)

**OpenShift Multiarch Tuning Operator** — configured by a **singleton**
`ClusterPodPlacementConfig` CRD (`multiarch.openshift.io/v1beta1`), which **must
be named `cluster`**. It deploys a pod-placement controller + webhook.

**Four-step scheduling-gate flow** per new pod:
1. Add the scheduling gate `multiarch.openshift.io/scheduling-gate` (prevents
   scheduling) — uses upstream **PodSchedulingGates**.
2. Compute the set of supported `kubernetes.io/arch` values by inspecting the
   pod's container image **manifest list (OCI image index)** — the intersection
   of arches available across the referenced images.
3. Inject that as a `nodeAffinity` requirement (`kubernetes.io/arch In [...]`).
4. Remove the scheduling gate → the pod schedules, and the stock scheduler
   `NodeAffinity` plugin enforces the arch constraint.

**Implication for us:** requires (a) kubelet labeling nodes `kubernetes.io/arch`
(rustkube-node), (b) apiserver support for **PodSchedulingGates** + admission
that injects the gate, (c) an operand/webhook that reads image manifest-lists,
(d) the existing scheduler NodeAffinity plugin. No scheduler-core change.

## OpenShift node placement & descheduler (partly verified)

- **Descheduler**: singleton `KubeDescheduler` CR (`operator.openshift.io/v1`,
  name `cluster`, ns `openshift-kube-descheduler-operator`) with
  `deschedulingIntervalSeconds` (default **3600**), `profiles`
  (AffinityAndTaints / TopologyAndDuplicates / LifecycleAndUtilization /
  CompactAndScale), `mode`, `evictionLimits`. Rebalances by evicting running
  pods (respecting PDBs) — separate from the scheduler.
- **Open (unverified):** exact merge/precedence of cluster `defaultNodeSelector`
  vs namespace `openshift.io/node-selector` vs pod `nodeSelector`, and the
  profile→strategy mapping. Believed to be an admission plugin that merges into
  `pod.spec.nodeSelector`.
