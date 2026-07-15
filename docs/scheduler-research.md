# kube-scheduler — upstream & OpenShift semantics (research for #3)

Implementation-oriented spec for the Rust `kube-scheduler` replacement, from a
deep, adversarially-verified research pass (25/25 claims confirmed 3-0 against
primary sources: kubernetes.io, version-pinned kubernetes/kubernetes source,
pkg.go.dev, OpenShift docs). Targets K8s 1.32 / OpenShift 4.x.

> **Version caveat:** default plugin list/weights verified at v1.30.1 (spot-checked
> v1.32.0); preemption at v1.32.2. Before coding, re-diff `default_plugins.go` and
> the preemption package against the exact 1.32.z patch targeted.

## 1. Framework (drop-in parity — well covered)

Every scheduling attempt = a **serial scheduling cycle** + a (possibly
concurrent) **binding cycle**. Extension points, in order:

- Scheduling: `PreEnqueue` → `QueueSort` → `PreFilter` → `Filter` → `PostFilter`
  → `PreScore` → `Score` (+ `NormalizeScore` sub-phase) → `Reserve` → `Permit`
- Binding: `PreBind` → `Bind` → `PostBind`

`Filter` removes infeasible nodes; `Score` ranks feasible ones; `PostFilter`
runs **only when no feasible node was found** (preemption is its use) and short-
circuits once a node is marked Schedulable. `QueueSort` allows exactly one
enabled plugin; `Bind` requires ≥1.

**Config:** `KubeSchedulerConfiguration` (`kubescheduler.config.k8s.io/v1`, GA
1.25) via `--config`. `parallelism` default **16**; `percentageOfNodesToScore`
0–100; a `profiles` list. Multiple profiles per instance (pod picks via
`.spec.schedulerName`), but **all profiles must share the same `queueSort`**
(single shared pending queue).

### Default-enabled plugins (MultiPoint, in order) + score weights
`SchedulingGates`, `PrioritySort`(queueSort), `NodeUnschedulable`(filter),
`NodeName`(filter), `TaintToleration`(filter/preScore/score, **w=3**),
`NodeAffinity`(filter/score, **w=2**), `NodePorts`(preFilter/filter),
`NodeResourcesFit`(preFilter/filter/score, **w=1**), `VolumeRestrictions`,
`EBSLimits`, `GCEPDLimits`, `NodeVolumeLimits`, `AzureDiskLimits`,
`VolumeBinding`(preFilter/filter/reserve/preBind/score), `VolumeZone`,
`PodTopologySpread`(preFilter/filter/preScore/score, **w=2**),
`InterPodAffinity`(preFilter/filter/preScore/score, **w=2**),
`DefaultPreemption`(postFilter),
`NodeResourcesBalancedAllocation`(score, **w=1**),
`ImageLocality`(score, **w=1**), `DefaultBinder`(bind). Unweighted → w=1.

`NodeResourcesFit` scoring strategies: **LeastAllocated** (default, spread),
**MostAllocated** (bin-pack), **RequestedToCapacityRatio** (custom curve).

### Priority & preemption
- `PriorityClass` value: 32-bit int ≤ 1e9 (>1e9 reserved for
  system-cluster/node-critical). Higher = higher priority; ties by enqueue time.
- `preemptionPolicy` default `PreemptLowerPriority`; `Never` = queue ahead but
  never preempt.
- **PriorityQueue**: `activeQ` (head = highest-priority pending), `backoffQ`
  (exponential backoff, **initial 1s / max 10s**), `unschedulablePods` (map, max
  **5m**). Constants: `DefaultPodInitialBackoffDuration=1s`,
  `DefaultPodMaxBackoffDuration=10s`, `DefaultPodMaxInUnschedulablePodsDuration=5m`.
- **DefaultPreemption** (PostFilter): `DryRunPreemption` simulates on a bounded
  random subset of nodes (`GetOffsetAndNumCandidates`: random offset +
  `calculateNumCandidates`). Per node, `SelectVictimsOnNode` finds a **minimal**
  victim set — remove all lower-priority pods to test feasibility, then sort
  high→low priority and reprieve (add back) with a **two-phase PDB-aware**
  reprieve (spare PDB-violating victims first). Candidate nodes that **don't**
  violate PDBs are preferred. A `Candidate` = nominated node + victim list. On
  preempting, set `pod.status.nominatedNodeName=N`; scheduler always tries the
  nominated node first but it's **not guaranteed** (can be cleared by a
  higher-priority arrival).

## 2. OpenShift divergence (partly covered)

**Verified:** cluster-wide **scheduler profiles** via the cluster-scoped
`Scheduler` CR (`config.openshift.io/v1`, name `cluster`, `spec.profile`):
- `LowNodeUtilization` (default; spread, upstream-like)
- `HighNodeUtilization` (bin-pack onto fewest nodes)
- `NoScoring` (disable all score plugins; low-latency)

## 3–4. GAPS — not yet verified (need a focused follow-up pass)

The adversarial pass did **not** surface confirmed claims for the areas you
emphasized. Treat as *not-yet-researched*, not absent:

- **OpenShift node placement**: cluster-wide `defaultNodeSelector`
  (scheduler.openshift.io) and namespace `openshift.io/node-selector` annotation
  — believed to be an **admission controller that mutates pod nodeSelector**, not
  scheduler logic; a drop-in scheduler must interoperate. Confirm the mechanism.
- **OpenShift Descheduler operator**: strategies/profiles (eviction of running
  pods to rebalance) — separate from the scheduler.
- **CPU-architecture / multi-arch scheduling**: `kubernetes.io/arch` &
  `kubernetes.io/os` node labels + `nodeAffinity`/`nodeSelector`; OpenShift
  **multiarch-tuning-operator** `ClusterPodPlacementConfig` inspects image
  **manifest-lists** to compute supported arches and injects arch-aware
  `nodeAffinity` (an admission/webhook flow, not scheduler-core). Confirm the
  end-to-end flow for mixed arm64/amd64.
- **Node draining & eviction**: `kubectl drain`/cordon, `node.spec.unschedulable`,
  the **policy/v1 Eviction subresource**, **PodDisruptionBudget** gating, graceful
  termination, DaemonSet/static-mirror/standalone/emptyDir handling,
  `--force`/`--ignore-daemonsets`/`--delete-emptydir-data`.

### Implementation implication
Arch-aware scheduling and OpenShift default-node-selectors are largely
**admission-time pod mutation** (inject nodeAffinity/nodeSelector), after which
the **stock scheduler's `NodeAffinity` filter/score already enforces them**. So
much of "arch-based scheduling" is an admission plugin + honoring
`kubernetes.io/arch` — not new scheduler code. Draining is a
controller/kubectl concern gated by the Eviction API + PDBs, not the scheduler.

## Primary sources
- kubernetes.io scheduling-framework, config-api/kube-scheduler-config.v1, scheduling/config
- kubernetes/kubernetes @v1.30.1 default_plugins.go; @v1.32.2 defaultpreemption
- pkg.go.dev scheduler/internal/queue, scheduler/framework/preemption
- OpenShift: nodes-scheduler-profiles, openshift/api config/v1/types_scheduling.go,
  openshift/enhancements descheduler-profiles, multiarch-tuning-operator docs
