# What upstream actually does, and what we should not build

Researched 2026-08-28 against `release-1.37` source, KEP `kep.yaml` files and
operator bindata — deliberately not against kubernetes.io, because the docs are
wrong in several load-bearing places. Full reference:
<https://claude.ai/code/artifact/161bea83-d1fd-40f6-bc85-8c6ab74be02e>

## Baseline

Current upstream is **v1.37 "Garhwal"** (2026-08-26). OpenShift 4.22 ships
**k8s 1.35**, two releases behind. Anything written against 1.31-era docs is
stale in ways that matter.

`kubernetes.io/docs/reference/scheduling/config/` **should not be used as the
plugin list**: it still names four cloud volume-limit plugins deleted in v1.32
and omits `SchedulingGates`, `DynamicResources` and `GangScheduling`. Read
`names.go` and `default_plugins.go` instead.

Extension points added since the commonly quoted list: **`PreBindPreFlight`**
(v1.34, mandatory for a faithful PreBind, undocumented on the website) and a
parallel PodGroup pipeline in v1.36–37 (`placementGenerate`, `placementScore`,
`podGroupPostFilter`, `PlacementFeasible`).

## NUMA: do not build a model in the scheduler

**The upstream scheduler is NUMA-blind in v1.37, and there has never been a KEP
to fix it.** The tracking issue was closed by a bot in 2020. The
NodeResourceTopology CRD is still `v1alpha2` after six years, its in-tree
staging repo is archived and retired, and the out-of-tree plugin is three
releases behind. Its own design doc concedes the scheduler cannot predict the
kubelet: the goal is statistical mitigation, not correctness.

So the plan is **not** a topology model in the scheduler. It is:

1. Set the kubelet: Topology Manager `single-numa-node`, **pod** scope, CPU
   Manager static with `full-pcpus-only`, and
   `prefer-align-cpus-by-uncorecache`. All GA, all free.
2. **Taint** the nodes with strict topology policies, so only workloads that
   want them land there.
3. Give `TopologyAffinityError` a **backoff penalty**, so a pod the kubelet
   rejected for topology is not immediately rescheduled onto the same node.

That is roughly 90% of the value for 2% of the code, and it is what the people
who tried the CRD route ended up doing.

**Cache/RDT-aware scheduling does not exist upstream.** KEP-3008 is closed and
rotten; the kubelet has no resctrl code. The only GA cache feature anywhere is
a single kubelet option (`prefer-align-cpus-by-uncorecache`). Anything we build
here would be ours alone to maintain, with no upstream to track.

Worth noting against our own position: **stormpump's `cpu.rs` already models
more than upstream exposes** — `Cpu { core, package, numa, l2, l3, kind }` and
a demand for whole cores on a given NUMA node sharing a last-level cache. The
gap has never been the node's model; it is that no scheduler can promise what
the kubelet will do.

## What to build, in order

1. **`max(spec, actuated, allocated)` resource accounting.** In-place pod
   resize is GA-*locked* in v1.35, so accounting from `spec.requests` alone is
   wrong on a shrink — the pod still holds the larger amount until the kubelet
   actuates. Also pod-level requests (beta-on in v1.34). *Our current
   accounting reads spec only, so this is unfinished rather than absent.*
2. **NUMA taints + `TopologyAffinityError` backoff**, per above.
3. **Topology spread rather than anti-affinity for spreading.** The argument is
   not performance — at our size that is irrelevant — it is that an existing
   pod's *required* anti-affinity taxes every pod scheduled after it, forever.
4. **VolumeBinding, including capacity scoring.** `StorageCapacityScoring`
   became default-on beta in v1.37, and this cluster is storage-heavy: it is
   the plugin most likely to matter here and least likely to be missed.
5. **QueueingHints and the three-queue model, keyed on entities.** The feature
   gate was deleted outright in v1.37 — this is simply how the scheduler works
   now, not an option.

## What would be a mistake to build

- **DRA.** Roughly 5,000 lines plus a faithful Kubernetes-CEL evaluator, to
  drive a GPU driver whose vendor calls it "not officially supported" and ships
  it disabled behind a flag named `gpuResourcesEnabledOverride`. There are no
  DRA storage drivers at all, and device plugins are stable and not deprecated.
  **Refuse** a pod with `spec.resourceClaims` rather than binding it into a
  permanent kubelet rejection.
- **Anything cache/RDT-aware in the scheduler**, per above.

## One landmine

If we ever run the descheduler: **at defaults it evicts pods with PVCs**, and
`nodeFit` (off by default) cannot see local-PV node affinity. On a storage
cluster that evicts a local-PV pod into permanent `Pending`.
