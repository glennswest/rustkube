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

### On the conformance VM, from the goldens (#140)

A run compiles nothing: it is a test workload, and it used to hold dev's
build slots for 45–90 minutes per chunk (four at once took dev to load 48 on
2026-09-27 and stalled every project's builds). The binaries are the
commit's **goldens** (owner, #140), and the suite runs on the conformance VM
(conform.g8.lo), which has no toolchain:

```bash
# 1. where the stormcentral CLI and a token are (a stormcentral session):
#    read the commit's goldens and deliver them to the VM
test/conformance/fetch.sh <sha>     # → DELIVERED conform@conform.g8.lo:/srv/conformance/<sha>

# 2. on the conformance VM: check out the same commit, run all six chunks
ssh conform@conform.g8.lo 'cd rustkube && git fetch -q && git checkout -q origin/main && test/conformance/vm.sh <sha>'
# → ~/results/<sha>/<chunk>.log and SUMMARY (passed/failed/skipped per chunk)
```

`fetch.sh` reads kube-apiserver, kube-controller-manager and
kube-scheduler from the rustkube-* stage goldens built at `<sha>` with
`stormcentral component fetch rustkube --commit <sha>` (stormcentral#557),
each checked against its build's sha256, and fastetcd from its newest golden
(`RK_FASTETCD_COMMIT` picks another). A commit with no golden is refused —
`stormcentral component build rustkube` makes one. It writes a `MANIFEST`
(the commit, the goldens read, every binary's sha256) and copies the
directory to the VM over ssh. The VM needs no stormcentral token.
`vm.sh` checks the directory against its MANIFEST and that it names `<sha>`,
runs the test scripts of `<sha>` from a worktree (`~/wt/<sha>`, so the
driver itself is the clone's, at main), and runs the chunks side by side.

The goldens are **release** builds; runs before 2026-10-21 used debug
binaries from the checkout and fastetcd's `main`, so timing-sensitive specs
may differ from those runs for that reason alone.

`RK_PORT_OFFSET` moves each rig's ports so chunks run side by side, and
`RK_SUITE_TIMEOUT` (`run.sh`'s default `100m`, `vm.sh`'s `45m`) cuts off specs
that hang; a chunk's real work is done in about 20 minutes. The `MANIFEST`
names the goldens and every binary's sha256, so a result names exactly what
it tested.

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
plane; they are the specs a run on real stormcos nodes is for, and they are
counted separately below.

What it does test is everything the control plane answers alone: API
machinery, RBAC and authentication, discovery, admission, the controllers that
work on API objects (namespaces, garbage collection, CronJobs, Services and
EndpointSlices, ResourceQuota), the scheduler's decisions, watch semantics.

## Results

### Focused reruns from goldens, 2026-10-28/29

After the d5f48f5 run, issues were closed on focused reruns on conform.g8.lo
from the 01a3affcd023 / dfb448b7c686 / 3e10ed08dcac goldens: all nine
`CustomResourcePublishOpenAPI` specs (3 of 3 runs; two passed at d5f48f5, #120),
ReplicationController "surface a failure condition" and the GC / quota specs
that use RCs (#125), startup-manifest LimitRange (#158) and the
EndpointSliceMirroring spec (#133). Specs that need a running Pod still fail
(no kubelet); preemption specs use an extended resource the fit filter ignores
(#297). The 306-failure table below has still not been triaged (#291).

### From the goldens, 2026-10-21, at d5f48f5 (#140)

The first run from goldens (`fetch.sh` then `vm.sh`; kube-apiserver
golden-rustkube-apiserver-4e63b7a9fbe4, controller-manager/scheduler
…-efab8dc99241, fastetcd golden-fastetcd-efab8dc99241 = v1.34.0+2; release
builds), six chunks side by side on conform.g8.lo: 429 specs reported,
**123 passed, 306 failed**.

| Chunk | Passed | Failed | |
|---|---:|---:|---|
| sig-api-machinery | 49 | 41 | suite timeout (45 m) |
| sig-apps | 8 | 47 | suite timeout (45 m) |
| sig-auth, cli, instrumentation, architecture, scheduling | 21 | 22 | |
| sig-network | 13 | 34 | |
| sig-node | 20 | 83 | |
| sig-storage | 12 | 79 | |
| **Total** | **123** | **306** | |

There is still no kubelet, so the pod-dependent specs fail as before. The
failures have not been triaged against the table below yet. #159's spec
(orphan RS created by deployment, PropagationPolicy Orphan) passed with all
six chunks running at once. Logs: `~/results/d5f48f5…/` on the VM.

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
  ServiceAccount one below. **Investigated in #159** (2026-10-07), from the
  code at efbea2d: its GC was a 30 s sweep that listed every kind in turn, so
  under six chunks an orphan delete could wait past the spec's 2 minutes
  (latency); and it judged an owner gone when the sweep's lists had not
  seen its uid, then deleted without preconditions — a Deployment created
  between the sweep's Deployment and ReplicaSet lists lost its ReplicaSet
  (unsafe). The #146 rewrite (d69e618, 2026-09-29) removed both: event-driven
  workers, owner absence confirmed by a GET of the owner, deletes with uid
  and resourceVersion preconditions, orphan membership confirmed by LIST
  before the finalizer goes. `test/e2e/gc-orphan-load.sh` runs the spec 20
  at once under churn; the conformance rerun needs #140.
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
| 11 | ResourceQuota: no controller or admission — #124 | implemented 2026-10-07 (controller, admission with charging, scopes); not rerun |
| 11 | ReplicationController: no controller — #125 | implemented 2026-10-07 (controller, `/scale`, defaults); not rerun — the specs that wait for Running/Ready Pods need a kubelet |
| 9 | CRD schemas not in `/openapi` — #120 | implemented 2026-10-07 (v2 definitions, v3 documents, stale versions dropped); not rerun |
| 8 | Validating/MutatingAdmissionPolicy — #119 (three hang to the suite timeout) | the four API-operations specs: served since 2026-10-07 (not rerun); the evaluation specs (which hang) are #234 |
| 5 | RuntimeClass — #135 | implemented 2026-10-07 (API, admission, scheduler overhead); not rerun — the Pod-runs specs need a kubelet |
| 4 | aggregated discovery — #107 | served 2026-10-07 (v2/v2beta1 JSON); not rerun |
| 4 | custom resource defaulting/pruning/fieldValidation — #121 | implemented after this run (watch events are not defaulted) |
| 4 | NodePort allocation, ClusterIP on type change — #132 | implemented 2026-10-07; not rerun (the functioning-NodePort spec also needs a node to reach it) |
| 4 | `resource.k8s.io/v1` (DRA) — #137 | served (CRUD); allocation is #225 |
| 3 | in-place pod resize — #136 | apiserver half since 2026-10-07 (`pods/resize`); the specs also need the kubelet's, rustkube-node#192 |
| 3 | `/scale` subresource — #86 | served since 2026-10-07; not rerun |
| 2 | API Priority and Fairness — #118 | served since 2026-10-07 (CRUD, `/status`, mandatory objects; not enforced); not rerun |
| 2 | YAML bodies, fieldValidation for built-ins — #122 | implemented 2026-10-07 (protos re-vendored to release-1.36); not rerun |
| 2 | ServiceCIDR/IPAddress — #134 | served since 2026-10-07 (CRUD, bootstrap ServiceCIDR); not rerun |
| 2 | scheduler records no Scheduled/FailedScheduling events — #138 | implemented after this run |
| 1 each | autoscaling/v1 #123; Table 406 #126; LimitRanger #131; EndpointSliceMirroring #133; store compaction / continue-token expiry #139 | implemented since 2026-10-07; not rerun |
| 6 | CSR `/approval` PATCH, Event `source` selector, all invalid sysctls in one error, Pod/PVC/PV `Pending` phase (#102), Ingress `/status` | wrong — fixed in 6d0ed10, after this run |
| 1 | default ServiceAccount not provisioned in time under six parallel chunks — the poll-and-list controllers (#66) | load |

"Needs a node" is not a pass: 242 specs have said nothing about the control
plane yet. They are what a run against real stormcos nodes is for (test
containers #96), and some of them will surface control-plane bugs this rig
cannot see.

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
runs validates current turbomode, or the features implemented since
2026-10-07. Repeating conformance for a new commit needs the golden delivery
path decided in #140 implemented (after stormcentral#557), then a runtime
environment appropriate to the tests. Passing API-only tests does not prove
node, network, storage or multi-master conformance.
