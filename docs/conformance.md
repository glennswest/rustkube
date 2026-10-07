# Conformance

The Kubernetes conformance suite is upstream's definition of "this is
Kubernetes": the `[Conformance]`-tagged specs of `e2e.test`, 443 of them in
v1.36. This page says how it is run against rustkube, what the run can and
cannot tell, and what it found (#67).

## How it runs

`test/conformance/run.sh [focus-regex]`, from the checkout:

- starts kube-apiserver, kube-controller-manager and kube-scheduler on a fresh
  fastetcd (the shared setup is `test/e2e/lib.sh`: a throwaway CA and serving
  cert, the store on tmpfs). With `RK_BIN`/`RK_FASTETCD` it uses prebuilt
  binaries; without them it builds both from source;
- fetches upstream's `e2e.test`, `ginkgo` and `kubectl` for the release matching the API
  posture the apiserver reports (1.36 — `stable-1.36.txt`), cached in
  `$HOME/target/k8s-e2e`;
- creates two **stand-in Nodes** — API objects with capacity, addresses and a
  Ready condition, kept Ready by renewing their Leases every 10 s the way a
  kubelet would;
- runs the `[Conformance]` specs 16 at a time and prints one
  `RESULT <passed|failed> <seconds> <name>` line per spec, with the failure
  message and where it failed.

### Historical VM procedure — to be replaced by goldens (#140)

A run compiles nothing: it is a test workload, and it used to hold dev's
build slots for 45–90 minutes per chunk (four at once took dev to load 48 on
2026-09-27 and stalled every project's builds). So the binaries are built
**once per commit** in one ordinary sc-build, and the suite runs on the
conformance VM, which has no toolchain:

The following records the old procedure, **not a working command sequence
under today's build-volume contract**:

```bash
# Historical only: stage.sh cannot publish this way now; goldens replace it (#140).
# 1. on the agent VM, from the checkout (pushed first): build and publish
sc-build test/conformance/stage.sh       # → STAGED /build/assets/conformance/<sha>

# 2. on the conformance VM: fetch, check out the same commit, run all six chunks
ssh conform@conform.g8.lo 'cd rustkube && git fetch -q && git checkout -q origin/main && test/conformance/vm.sh <sha>'
# → ~/results/<sha>/<chunk>.log and SUMMARY (passed/failed/skipped per chunk)
```

> **Staging is broken since 2026-09-28.** sc-build now gives every job a
> private drive and keeps nothing, so `stage.sh` can no longer write
> `/build/assets` (#140). The owner has decided the route: **goldens** —
> conformance binaries reach the VM as the component's golden artifacts.
> That is not implemented yet: `run.sh`/`vm.sh` still expect the staged
> directory, and `stage.sh` still writes `/build/assets`. Until it is, only an
> already-staged commit can be run; efbea2d is still staged on the VM.

`vm.sh` fetches the staged directory with a read-only rsync key (it can read
`/build/assets/conformance` on dev and nothing else), checks the binaries
against their MANIFEST, and runs the chunks side by side:

`RK_PORT_OFFSET` moves each rig's ports so chunks run side by side, and
`RK_SUITE_TIMEOUT` (default `100m`) cuts off specs that hang; a chunk's real
work is done in about 20 minutes. `stage.sh` writes a `MANIFEST` with both
commits and every binary's sha256, so a result names exactly what it tested.

To chase one failure, focus on it and set `RK_LOG_GREP` to a regex: the
matching apiserver and controller-manager log lines are printed at the end.
`RK_WHY_LINES=40` prints that many more lines of each failure message, which
is where a `Failf` with a diff puts the diff.

## What this run can and cannot say

**There is no kubelet.** The Nodes exist only as objects, so the scheduler
places pods and nothing ever runs them. Every spec that needs a running pod —
most of `sig-node`, `sig-storage` and `sig-network`, the webhook and
conversion specs that deploy a server pod, anything reading logs or exec'ing —
fails on its pod-start timeout. Those failures say nothing about the control
plane; they are the specs a run on real stormcos nodes is for
(rustkube-node#27, #32 stand in front of that), and they are counted
separately below.

What it does test is everything the control plane answers alone: API
machinery, RBAC and authentication, discovery, admission, the controllers that
work on API objects (namespaces, garbage collection, CronJobs, Services and
EndpointSlices, ResourceQuota), the scheduler's decisions, watch semantics.

## Results

### The first run on the conformance VM, 2026-09-28, at efbea2d

The same six chunks, on conform.g8.lo from staged binaries (`vm.sh`): 428
specs reported, **75 passed, 353 failed** — fewer passes than 430b268, and
not because of the control plane:

| Chunk | 430b268 | efbea2d | after the kubectl fix |
|---|---:|---:|---:|
| sig-api-machinery | 19 | 18 | — |
| sig-apps | 7 | 7 | — |
| sig-auth, cli, instrumentation, architecture, scheduling | 18 | 12 | **19** |
| sig-network | 10 | 11 | — |
| sig-node | 14 | 15 | — |
| sig-storage | 11 | 12 | — |

- **Seven sig-cli specs failed on the VM's missing `kubectl`** ("executable
  file not found"); the build box had one on PATH. `run.sh` now fetches the
  release's own `kubectl` (d1881ac). Rerun of that chunk on the VM with the
  efbea2d binaries and the fixed script: 19 passed, 24 failed — all seven back,
  plus the CSR spec fixed in 6d0ed10.
- **The four fixes of 6d0ed10 were confirmed** end to end: CSR API operations,
  Ingress API operations, invalid sysctls, PV/PVC status.
- **GC "orphan RS created by deployment"** failed once under six parallel
  chunks and passed four times out of four focused, one of them alongside a
  full chunk: timing under load, not a regression. Counted as "load" with the
  ServiceAccount one below.
- **PriorityClass endpoints** was reached for the first time (#85 put the kind
  in discovery) and found `value` writable; it is immutable upstream. Fixed in
  c241688 — PriorityClass `value` and `preemptionPolicy` refuse change (422).

### The run of 2026-09-27, at 430b268

e2e.test v1.36.5, every `[Conformance]` spec (443), in the six chunks above.
This was the last run made inside dev's build slots; the next ones run on the
conformance VM. 433 specs reported (the rest were still queued when a chunk's
suite timeout struck): **79 passed, 354 failed**.

| Chunk | Passed | Failed |
|---|---:|---:|
| sig-api-machinery | 19 | 71 |
| sig-apps | 7 | 53 |
| sig-auth, cli, instrumentation, architecture, scheduling | 18 | 24 |
| sig-network | 10 | 37 |
| sig-node | 14 | 89 |
| sig-storage | 11 | 80 |
| **Total** | **79** | **354** |

The same chunks at 2e5dcb9, earlier that day, passed 58; the difference is
the fixes listed below.

Every failure, by cause (`tmp/classify.py`-style rules over the `RESULT`/
`WHY` lines; every one was read):

| Failures | Cause | Kind |
|---:|---|---|
| 242 | needs a running pod: pod-start, Deployment/DaemonSet readiness, DNS, pod networking, logs, init containers | needs a node |
| 26 | admission/conversion webhook and aggregator specs: their server is a pod (and then #82, #83) | needs a node, then not implemented |
| 11 | ResourceQuota: no controller or admission — #124 | not implemented |
| 11 | ReplicationController: no controller — #125 | not implemented |
| 9 | CRD schemas not in `/openapi` — #120 | not implemented |
| 8 | Validating/MutatingAdmissionPolicy — #119 (three hang to the suite timeout) | the four API-operations specs: served since 2026-10-07 (not rerun); the evaluation specs (which hang) are #234 |
| 5 | RuntimeClass — #135 | not implemented |
| 4 | aggregated discovery — #107 | not implemented |
| 4 | custom resource defaulting/pruning/fieldValidation — #121 | implemented after this run (watch events are not defaulted) |
| 4 | NodePort allocation, ClusterIP on type change — #132 | not implemented |
| 4 | `resource.k8s.io/v1` (DRA) — #137 | served (CRUD); allocation is #225 |
| 3 | in-place pod resize — #136 | not implemented |
| 3 | `/scale` subresource — #86 | served since 2026-10-07; not rerun |
| 2 | API Priority and Fairness — #118 | not implemented |
| 2 | YAML bodies, fieldValidation for built-ins — #122 | implemented 2026-10-07 (protos re-vendored to release-1.36); not rerun |
| 2 | ServiceCIDR/IPAddress — #134 | served since 2026-10-07 (CRUD, bootstrap ServiceCIDR); not rerun |
| 2 | scheduler records no Scheduled/FailedScheduling events — #138 | implemented after this run |
| 1 each | autoscaling/v1 #123 (served since 2026-10-07, not rerun); Table 406 #126 (since 2026-10-07, not rerun); LimitRanger #131 (enforced since 2026-10-07, not rerun); EndpointSliceMirroring #133; store compaction / continue-token expiry #139 | not implemented |
| 6 | CSR `/approval` PATCH, Event `source` selector, all invalid sysctls in one error, Pod/PVC/PV `Pending` phase (#102), Ingress `/status` | wrong — fixed in 6d0ed10, after this run |
| 1 | default ServiceAccount not provisioned in time under six parallel chunks — the poll-and-list controllers (#66) | load |

"Needs a node" is not a pass: 242 specs have said nothing about the control
plane yet. They are what a run against real stormcos nodes is for
(rustkube-node#27, #32; test containers #96), and some of them will surface
control-plane bugs this rig cannot see.

### Found and fixed by the runs

The first runs (2026-09-26/27), in the control plane:

- field selectors compared absent fields as missing — no node matched
  `spec.unschedulable=false`, so the suite found no schedulable node;
- no `kube-root-ca.crt` publisher — every test's namespace setup waited for it;
- a LIST could miss the caller's own just-made write (read-your-writes);
- PUT created missing objects and let a body rewrite `uid` and
  `creationTimestamp`; a patch could delete `creationTimestamp`;
- namespace termination starved default-ServiceAccount provisioning;
- `?` in a cron schedule, `/apis/` and `/apis/{group}` discovery, empty
  Service `type`, and protobuf schemas for TokenRequest/SubjectAccessReview.

From the 2026-09-27 triage (CHANGELOG has each):

- **protobuf creates stored an empty namespace**, so client-go's next request
  went to a namespace-less path — every webhook spec's server Deployment;
- **generateName was ignored** — the second create by generateName collided;
- **impersonation was ignored** — `kubectl --as` ran with the caller's rights;
- a watch with no `resourceVersion` sent no initial state; a selector watch
  sent no DELETED/ADDED as objects left or entered it;
- LIST items had no `resourceVersion` (#111), paged lists no pinned revision
  or `remainingItemCount`; `deletecollection` was a 405 everywhere;
- CRD schemas created over protobuf lost `x-kubernetes-*`, `$ref`, `$schema`;
- no ServiceAccount token volume; no ConfigMap/Secret key, immutability or
  sysctl validation; no pod `qosClass`; status writes dropped their label and
  annotation changes;
- list kinds for PodTemplate/ReplicationController/ResourceQuota/LimitRange;
  events.k8s.io over protobuf; VolumeAttributesClass; EndpointSlice
  `managed-by`; a DELETE lost to a concurrent write was a 409.

Some fixes after 430b268 were confirmed by the efbea2d run and focused
reruns above; others have unit/API-rig evidence only. None of these historical
runs validates current turbomode. Repeating conformance for a new commit
needs the golden delivery path decided in #140 implemented, then a runtime
environment appropriate to the tests. Passing API-only tests does not prove
node, network, storage or multi-master conformance.
