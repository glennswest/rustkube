# rustkube-test — rustkube's test container

rustkube's tests of a **running** control plane, per stormcentral's
[`docs/test-standard.md`](https://github.com/glennswest/stormcentral/blob/main/docs/test-standard.md)
(#96). Unit tests stay in `cargo test`; the conformance suite is
[docs/conformance.md](../docs/conformance.md). rustkube's tests live here, not
in stormcos_qa (owner, 2026-09-28: "tests per component, and only system burn
or stress tests of the system in the qa"; #35).

## How it runs

stormcentral builds the image from this repo at a commit (`test/build.sh`
stages a static musl binary, `test/Containerfile` packages it `FROM scratch`),
pushes it to the test machine's registry, and runs it as a Job:

```bash
stormcentral test run rustkube short     # or medium, long; --tag <machine>
```

in a namespace of its own, `test-rustkube-<suite>-<run id>`, as the ServiceAccount
`storm-test`, whose Role is `*` in that namespace **and nothing else**. So
every check works in its namespace; the cluster-scoped ones (CRDs, Nodes,
leader-election Leases) are reported as a skip. It talks HTTPS to
`$STORM_API`, verified with the ServiceAccount's `ca.crt`, with its token.

The pods the suites create run this same image as `/test idle` (unprivileged,
8Mi, no API token), so nothing is pulled from outside the cluster.
`/test idle 0` exits at once; a Job's pod runs that.

Output: one JSON object per test on stdout —
`{"test": …, "status": "pass|fail|skip", "ms": …, "detail": …}` — then
`{"summary": {"pass": n, "fail": n, "skip": n}}`. Exit 0 all passed, 1 a
test failed, 2 the suite could not run (reported as a `could-not-run` skip).

## The suites

**short** (< 2 min) — the control plane is up and does its main job:

| test | what it proves |
|---|---|
| `apiserver-ready` | `/readyz` is `ok` over TLS verified with the cluster CA; the token is accepted |
| `object-roundtrip` | a ConfigMap created, read back, updated (resourceVersion rises — the store's revision), a stale update is 409, deleted |
| `watch-sees-writes` | a watch from the list's revision sees ADDED and DELETED |
| `namespace-provisioned` | the controller-manager made the run namespace's `default` ServiceAccount and `kube-root-ca.crt` |
| `replicaset-pod-bound` | a ReplicaSet's pod is created (controller-manager) and bound to a node (scheduler) within 60 s |

**medium** (< 30 min) — features and failure paths, end to end. The checks of
the `stormcos_qa/tests/rustkube/*.sh` scripts that fit a namespace (ported
here; their retirement there is stormcos_qa#25, still open), and the
regressions fixed since:

| test | what it proves |
|---|---|
| `server-side-apply` | apply upserts, `managedFields` names the manager, a conflicting manager gets 409, `force` takes the field |
| `json-patch-test-null` | `test` of null against an absent path holds; a failing `test` refuses the patch |
| `strategic-merge-by-key` | a Deployment's containers merge by name |
| `status-write-conditional` | a stale `/status` PUT is 409 and changes nothing (#78) |
| `delete-options` | a wrong uid precondition is 409; `?dryRun=All` keeps the object; the right uid deletes |
| `label-field-selectors` | equality, existence + inequality, and a field selector |
| `list-paging` | 7 items in 3 pages of 3, each with its resourceVersion (#111) |
| `generate-name` | two creates by generateName get two names |
| `watch-initial-events` | WatchList: the objects, then the `initial-events-end` bookmark |
| `watch-selector-leave` | a selector watch sees DELETED when an object stops matching (#100) |
| `partial-object-metadata` | `as=PartialObjectMetadataList` returns metadata only |
| `events-translation` | a core/v1 Event reads as events.k8s.io/v1 |
| `cluster-scoped` | skip: CRD lifecycle, Nodes Ready, Leases need more than the run's Role |
| `gc-orphan` | an Orphan delete keeps the ReplicaSet and removes its ownerReference |
| `gc-foreground` | a Foreground delete removes the ReplicaSet and pods before the Deployment |
| `deployment-rollout-running` | 2 replicas Ready, then a template change rolled out |
| `service-endpointslice` | the Service's EndpointSlice lists both Ready pods |
| `job-completes` | a Job's pod runs and exits 0; the Job is Complete |
| `daemonset-steady` | one pod per node, Ready, and the same pods 20 s later |

The last four need a kubelet that runs the pods — they do on a test machine.

**long** (the night window) — waves, per the standard's overnight soaks, of
the control plane's own workload: each wave ramps ConfigMaps (sixteen
writers, four watches following), a 10-pod Deployment, updates every object,
lists them in pages, drains with a `deletecollection`, and checks nothing is
left. Wave 1 is 200 objects; later waves about a minute of writes at wave 1's
rate (at most 5000), varied ×½/×1/×1½. One line per wave with its numbers,
then `trend`: fail on residue, missed watch events or failed updates in any
wave, create p50 of the last three waves over twice wave 1's, or the
apiserver's resident memory (its `/metrics`) growing by half after wave 2.
Pod waves at the machine's capacity are system stress and belong to
stormcos_qa.

## The e2e rigs: `rigs` and `rigs-night` (#173)

The regression rigs in `test/e2e/` start their own fastetcd and control plane
and drive them; they used to run inside build slots (`sc-build 'bash
test/e2e/…'`), which the build rules forbid ("Test workloads never hold a
build slot") and the owner answered on #162: "Persistent tests should be
pods, and should live on forge." They are now two suites of this image, run
like the others:

```bash
stormcentral test run rustkube rigs --tag <machine>        # day: ≤ 30 min
stormcentral test run rustkube rigs-night --tag <pve VM>   # night window, ≤ 2 h
```

- `test/build.sh` also stages the commit's release (static musl)
  `kube-apiserver`, `kube-controller-manager`, `kube-scheduler`, fastetcd
  at `RK_FASTETCD_REF`, and the upstream tools some rigs drive: `oc`,
  `kubectl`, the CSI hostpath driver, external-provisioner, external-resizer,
  snapshot-controller (copied out of their registry.k8s.io images by
  `test/fetch-image-file.py`, no podman) and the snapshot CRDs/RBAC. The
  versions are `test/e2e/versions.sh`, which the rigs read too. So the pod
  needs neither cargo, podman nor the internet.
- `/test rigs|rigs-night` execs `test/rigs.sh` (`/rigs/run.sh` in the image):
  each rig, in turn, in a scratch copy under `$TMPDIR`, with `RK_BIN`,
  `RK_FASTETCD` and `RK_TOOLS` pointing at the staged files. One JSON line
  per rig, `rig:<name>`, pass/fail with its time and its first `FAIL` lines;
  the whole output in `/results/<name>.log`. A rig that cannot start (exit
  100) is a fail with "could not start"; if none could, the suite exits 2.
  Rigs not started before the budget runs out are skips.
- Which rig is in which suite is in `test/rigs.sh`: `rigs` the functional
  ones (a minute or two each), `rigs-night` the idle windows, latency under
  load, failover and cert-reload ticks. `test-container.sh` is in neither: it
  is this image's own short/medium against a rig, and runs by hand.
- A rig's output ends with its failures (#181): `lib.sh` keeps a copy of
  everything the rig prints (`$W/rig.out`), and `report()` prints, after the
  apiserver/datastore/controller-manager log tails, `---- failed checks (N):`
  and every `FAIL  …` line — the bash checks' and the ones a rig's Python
  printed. So the end of a log, and an excerpt or issue made from it, names
  what failed rather than 80 lines of fastetcd INFO.
- `test/requires.toml` declares both with `budget_secs` (#247). No privilege,
  host path or cluster read: the rigs listen on the pod's loopback.

`sc-build` keeps `cargo build && cargo test`. A rig may still be run by hand
on a workstation-like box, but not in a build slot.

## Requirements

Nothing about the machine: no devices, sizes or node names. It needs the
node's apiserver, a scheduler with a Ready node to bind to, and (for the last
four medium checks and long's pods) a kubelet. `requires: []`.

## By hand

`test/e2e/test-container.sh [suites]` runs this image's short/medium against
a rig. Like every rig it is a test workload: not for a build slot (#173).

`test/e2e/test-container.sh` plays stormcentral's runner (namespace,
`storm-test` + namespaced Role, its token via `STORM_SA_DIR`) against two
stand-in Nodes with no kubelet: every check that needs no running pod must
pass there. Outside a pod, `STORM_SA_DIR` names a directory holding `token`,
`ca.crt` and `namespace`, and `RUSTKUBE_TEST_IMAGE` the image for workload
pods.

## Latest acceptance attempt — 2026-09-29

`sc-build 'cargo build --locked -p rustkube-test && cargo test --locked -p rustkube-test'`
at a4dba9c compiled the test program and passed all nine unit tests.
The remote command exited 0; the wrapper could not append its local build
history because that filesystem was read-only.

`stormcentral test run rustkube short --commit d7bc1f8a757ff5aeeb6dc997f8856dd26a92ec09`
was refused with HTTP 400 before a Job or run ID was created: the only
registered test machine, C2NR0Q2, had three previous runs fail waiting for its
apiserver. Its last install was 11.52, failed. This is infrastructure failure,
not a suite result. Recovery is tracked by
[stormcentral#63](https://github.com/glennswest/stormcentral/issues/63).
Retry the short suite through stormcentral after recovery; #96 stays open
until that real-machine run passes. No hardware-specific assumptions were
added to the suite.

`e2e/indexed-selectors.sh` exercises Service/PDB Pod relabelling, namespace
isolation, negative selectors, idle writes and UID replacement against the
API/store rig (suite `rigs`); it does not need a kubelet. The shared rig
keeps its store and scratch under `$TMPDIR`.

`e2e/indexed-safety.sh` checks GC propagation, Event expiry, namespace
finalization and scheduler burst/reservation accounting on the same disposable
rig. It uses stand-in Nodes and does not claim real-node performance.

`e2e/schedule-latency.sh` times Pod creation to binding on one stand-in
Node, from the Pod's ADDED event to the first event carrying `nodeName` on
the same watch. It creates five pods 4 s apart, a burst of 20 and ten more
1 s apart. Bounds: p50 < 20 ms, the scheduler's own share (ADDED →
`storm.io/scheduled-at`) p99 < 10 ms, and no growth. Each pod's split (create
POST, scheduler, bind write → seen) is printed. End-to-end p99 is bounded only
with `RK_SCHED_P99_MS`, because on the shared build box the bind write's
datastore time stalls under other jobs' I/O. Run with `RK_RELEASE=1` (#190).

`e2e/cr-status.sh` (#128, the flowsdn audit's C12) creates four CRDs —
namespaced and cluster-scoped, with and without `subresources.status` — and
over HTTP checks that, with the subresource, main POST drops status and main
PUT, merge PATCH, JSON PATCH and server-side apply change spec but keep the
stored status, apply-create drops it, `/status` PUT and PATCH change status
and keep spec, and a stale `/status` PUT is a 409; without it, POST, PUT and
PATCH store status as an ordinary field. It also follows
`metadata.generation` (#198): 1 on create, +1 per spec write (or, without the
subresource, per write outside metadata), untouched by `/status`, by
metadata-only writes and by a client-sent value.

`e2e/requester.sh` (#210) creates `storage.storm.io` CRDs (cluster and
namespaced) and checks that a create by alice, with forged
`storage.storm.io/requester[-groups]`, is stamped alice and her groups; that
bob's PUT, merge PATCH, JSON PATCH remove, server-side apply and `/status`
PUT cannot change or drop the stamp; that bob's apply-create is stamped bob;
and that another group's object keeps the annotation its client wrote.

`e2e/wffc-latency.sh` (#147, suite `rigs`) times 25 Pods, each with a fresh
`stormblock` WaitForFirstConsumer claim, from create to bound (the
scheduler's µs `storm.io/scheduled-at`) through selected-node, the stormblock
PV, the binder and the bind, on a stand-in Node with no kubelet: all bound,
claims Bound to `pvc-<ns>-<claim>`, and p99 < 1 s.

`e2e/dra-crud.sh` (#137, suite `rigs`) is what the conformance suite's four
`[DRA] CRUD Tests resource.k8s.io/v1` specs exercise: discovery of the group
and its four kinds (+ `resourceclaims/status`), and for each kind create,
get, list (and across namespaces), a watch's ADDED, merge patch, update,
protobuf GET and delete; a ResourceClaim allocation written through
`/status` with spec untouched; deletecollection by label.

`e2e/multi-master.sh` (#149, suite `rigs-night`) is three masters on one
host: a 3-member fastetcd Raft cluster (`RK_ETCD_MEMBERS=3` in `lib.sh`),
three apiservers (`RK_APISERVERS=3`, `start_apiserver n`), two electing
controller-managers and two electing schedulers on different apiservers, a
stand-in Node. It checks writes through one apiserver read through the
others; a LIST paged across three apiservers is one snapshot; a watch moved
to another apiserver at its last revision loses and repeats nothing; the
controller-manager leader paused past its lease (standby scales, resumed
leader creates nothing extra); the scheduler leader killed (no node
overcommitted); an apiserver killed (GC still works); quorum loss (writes
refused, nothing acknowledged lost); a fresh controller-manager rebuilds.
Takeover times are printed. A stopped process stands in for a partition.

`e2e/hpa-v1.sh` (#123, suite `rigs`): autoscaling v2 (preferred) and v1
in discovery; a v1 create stored as v2's cpu metric; a v2 object with a
memory metric and behavior read as v1 with upstream's annotations; a v1
PATCH of the CPU target keeping the rest; a v1 GET + PUT leaving v2 as it
was; v1 list and watch carrying v1 objects; delete through v1.

`e2e/gc-orphan-load.sh` (#159, suite `rigs`): the GC orphan conformance
spec 20 times at once while other namespaces churn Deployments — ReplicaSet
uids, resourceVersions, owner references and delete-to-gone times per run;
no orphaned ReplicaSet deleted (also watched for DELETED), every Deployment
gone within 120 s, references cut, ReplicaSets still there 15 s later.

`e2e/resource-quota.sh` (#124, suite `rigs`): quota status computed,
Pods charged at once, exceeded / must-specify 403s, counts for services,
secrets and replicasets, usage lowered on delete, BestEffort scope, six
concurrent creates against pods=3 admitting three.

`e2e/runtime-class.sh` (#135, suite `rigs`): RuntimeClass API operations,
admission (missing/deleted class 403, overhead and scheduling from the
class, mismatched or classless overhead 403), and the scheduler counting
overhead on a 1-CPU stand-in node.

`e2e/crd-openapi.sh` (#120, suite `rigs`): CRD schemas in `/openapi/v2`
(the conformance equality check) and `/openapi/v3`, a schema-less CRD,
`kubectl explain`, a renamed and an unserved version, a deleted CRD.

`e2e/manifest-admission.sh` (#158, suite `rigs`): objects from
`--manifest-dir` get the API's admission — a Pod's defaults, LimitRange,
QoS; a NodePort Service's allocations; Secret stringData; an invalid
ConfigMap refused; a CR's schema default and generation — and after a
restart a Reconcile keeps the Service's allocations and refuses an immutable
ConfigMap's change.

`e2e/vmi-verbs.sh` (#141, suite `rigs`): the five VMI verbs against a stub
kubelet on 127.0.0.1:10250 — path, bearer, query and answer passed through,
dryRun not forwarded, 409/404 cases, `edit` allowed and `view` refused.

`e2e/replication-controller.sh` (#125, suite `rigs`): defaults from the
template, Pods owned by kind ReplicationController, status, `/scale` GET /
PUT / PATCH, a deleted Pod replaced, orphan and background deletes.

`e2e/service-nodeport.sh` (#132, suite `rigs`): node ports for NodePort
and LoadBalancer, named/taken/out-of-range/ClusterIP-named ports, TCP+UDP on
one number, PUT keeping allocated values, immutable ClusterIP, the type
changes (to ClusterIP frees ports, to ExternalName drops the ClusterIP, from
ExternalName allocates), delete frees.

`e2e/ephemeral-volume.sh` (#94, suite `rigs`): a Pod's generic ephemeral
volume gets `<pod>-<volume>` from its template, owned by the Pod; a foreign
claim of that name is left alone with a Warning Event; the claim goes with
the Pod.

`e2e/scheduler-preemption.sh` (#84, suite `rigs`): a high-priority Pod on a
full 1-CPU node evicts exactly one low Pod (Preempted Event), binds and
loses its nominatedNodeName; Never and equal priority preempt nothing; a
PDB-protected Pod is spared for an unprotected victim.

`e2e/stormblock-class.sh` (#92, suite `rigs`): a `stormblock` class with a
CSI provisioner gets no in-kubelet PV; recreated with `stormblock.storm.io`,
the claim gets its PV.

`e2e/node-proxy.sh` (#108, suite `rigs`): nodes/n1/proxy against a stub
kubelet — path, query, bearer, POST body, 404 passthrough, kubectl get --raw,
nodes/proxy RBAC (403 without, 200 with), unknown node 404.

`e2e/whoami.sh` (#116, suite `rigs`): selfsubjectreviews in discovery;
`kubectl auth whoami` as a plain user and the admin; a raw POST's 201.

`e2e/csr-signers.sh` (#199, suite `rigs`): the CSR controller signs the
kubelet-client and (hand-approved) kube-apiserver-client signers, and leaves
an external signerName (kubelet-serving here) and `stormcert.io/*` unapproved
or unsigned.

`e2e/status-put-latency.sh` (#191, suite `rigs`): 300 sequential pod
status PUTs on an idle apiserver, p50/p99/max printed, p99 under 50 ms; the
write-phase histogram has every phase; a webhook sleeping 300 ms makes the
breakdown log `slow write` with `webhooks_ms` ≥ 250, plus `slow request`.

`e2e/notfound-message.sh` (#109, suite `rigs`): 404s name the object as the
API does (`namespaces "x"`, `deployments.apps "web"`, a custom resource's
`widgets.<group>`) with Status `details`, never the storage key; an
unregistered CR type is "could not find the requested resource".

`e2e/unserved-resource.sh` (#110, suite `rigs`): GET/POST/PUT of unserved
resources in core, apps and rbac are 404 and store nothing; RC, quota and
pod-template lists carry their kinds; namespace and pods/log paths still
reach their handlers; a CRD in a built-in group is served.

`e2e/watch-timeout.sh` (#165, suite `rigs`): built-in, WatchList and
custom-resource watches end cleanly at `timeoutSeconds`, an event before the
deadline arrives first, no `timeoutSeconds` stays open, a non-integer is 400.

`e2e/endpointslice-mirroring.sh` (#133, suite `rigs`): the conformance
spec's create/update/delete of a custom Endpoints mirrored into a slice
(owner, labels, same slice on update), an IPv6 address in its own slice,
skip-mirror, a selector added, the Service deleted.

`e2e/pod-resize.sh` (#136, suite `rigs`): `pods/resize` in discovery;
`kubectl patch --subresource=resize` changes resources/resizePolicy and not
the image; PUT with and with a stale resourceVersion; 422 for a QoS change,
a removed request, a non-cpu/memory resource, request > limit, a non-sidecar
init container; a running Pod without status resources refused, with them
resized.

`e2e/flowcontrol.sh` (#118, suite `rigs`): flowcontrol.apiserver.k8s.io/v1
in `/apis` and discovery, the mandatory exempt/catch-all objects, and per
resource create, get, list, watch, merge patch, PUT, `/status` patch/get,
protobuf GET, delete and deletecollection.

`e2e/table-review.sh` (#126, suite `rigs`): a Table-only Accept on
SelfSubjectAccessReview, SubjectAccessReview and TokenReview is 406
NotAcceptable; with kubectl's JSON fallback they answer; a pods LIST still
gets its Table.

`e2e/limitrange.sh` (#131, suite `rigs`): the conformance spec's
LimitRange — defaults and the `kubernetes.io/limit-ranger` annotation on a
Pod with none, a partial Pod merged, below-min / above-max refused 403, the
range relaxed admits it, a PVC below `min` refused, requests defaulted from
limits without a LimitRange.

`e2e/admission-policy-api.sh` (#119, suite `rigs`): the four
admission policy resources in discovery, cluster-scoped; each created (with
the not-enforced Warning), read, listed, patched, read over protobuf and
deleted by collection; ValidatingAdmissionPolicy's `/status` patched with
its spec kept.

`e2e/field-validation.sh` (#122, suite `rigs`): the conformance
FieldValidation bodies — Strict refuses an unknown + duplicate field and
unknown metadata with upstream's message; Warn creates and warns; Ignore
creates silently; a valid Deployment and a Pod with 1.34 pod-level
`resources` pass Strict; a POST with no Content-Type is JSON; a YAML body is
accepted, and a repeated YAML key under Strict is 400.

`e2e/service-cidr.sh` (#134, suite `rigs`): discovery lists servicecidrs
(+/status) and ipaddresses, cluster-scoped; the bootstrapped `kubernetes`
ServiceCIDR holds `--service-cidr`, Ready; ServiceCIDR create/list/patch/
`/status`/delete; IPAddress create, a protobuf GET, list, patch, delete.

`e2e/vmi-migration.sh` also covers `addedNodeSelector` (#208): it forces
the second-choice node, one no node meets is `TargetScheduled=False`, and a
non-string value is 422.

`e2e/scale.sh` (#86, suite `rigs`): `kubectl scale` on a Deployment,
ReplicaSet, StatefulSet and a CR with `subresources.scale`;
`--current-replicas` mismatch refused; the Scale's shape; stale PUT 409,
negative 422, merge PATCH; a CRD without scale 404; discovery entries.

`e2e/scheduling-gates.sh` (#87, suite `rigs`): a Pod with two gates stays
unbound with `PodScheduled=False/SchedulingGated` and no FailedScheduling
Event; adding a gate is 422; removing one leaves it waiting, removing the
last binds it; an ungated Pod binds at once.

`e2e/hpa-metrics.sh` (#89, suite `rigs`): a stub cadvisor answers
`/api/v1.3/subcontainers/` with the root cgroup and labelled containers for
the namespace's Pods (the rig marks Pods Running/Ready: no kubelet).
NodeMetrics and PodMetrics come from it (sandbox left out); an HPA at 50 %
of a 100m request with Pods at 200 % scales the Deployment to maxReplicas 5
(`currentMetrics` 200, `ValidMetricFound`, `ScalingLimited`), back to 1 at
0 usage, and with cadvisor gone reads `FailedGetResourceMetric` and leaves
the count.

`e2e/metrics-auth.sh` (#90, suite `rigs`): the apiserver's `/metrics` is
401 without a token, 403 without a grant, 200 for `system:monitoring`, with
`_bucket` series and no summaries and `process_cpu_seconds_total` a counter;
the controller manager and scheduler (`--tls-cert-file`, via `RK_CM_ARGS` /
`RK_SCHED_ARGS`) refuse plain HTTP, serve health to anyone, and answer
`/metrics` 401/401/403/200 for no token, an unknown token, no grant and
`system:monitoring`; reconcile histograms exist and the leader gauge is 1.
Rigs that scrape `/metrics` send the admin token.

`e2e/service-create.sh` (#113, suite `rigs`) times Service creates: 100
sequential (each under 1 s, the last 20 no slower than twice the first 20)
and 25 concurrent (all 201 within 10 s), checks 125 distinct ClusterIPs
none in the bottom 256 of the /12, a fixed address there granted, and a taken
one refused (422).

`e2e/aggregation.sh` (#83, suite `rigs`): a Python stub aggregated API
server behind Service `agg/stub` (ClusterIP 127.0.0.9, caBundle the rig's
CA, front-proxy client certificate required). The APIService goes
`Passed`; the group is in `/apis` and `/apis/{group}`; LIST, POST and a
streamed watch are proxied with the front-proxy certificate and the
caller's X-Remote-User/Group (forged ones and Authorization dropped); RBAC
is enforced here first; protobuf Accept passes untranscoded;
extension-apiserver-authentication and the delegator roles exist; an
APIService for `apps/v1` changes nothing; a missing Service is
`ServiceNotFound`; a stopped backend is `FailedDiscoveryCheck` and 503.

`e2e/bad-token.sh` (#115, suite `rigs`) runs the apiserver with
`--anonymous-auth true`: no credentials and an empty `Bearer ` token are
anonymous (discovery 200, namespaces 403); a garbage token, a JWT not signed
by the cluster and a lower-case `bearer` garbage token are 401 with a
`Status`, reason `Unauthorized`; the admin token is 200. `RK_ANONYMOUS_AUTH`
in lib.sh sets the flag (default false).

`e2e/compaction.sh` (#139, #127, suite `rigs`) runs the apiserver with
`--etcd-compaction-interval=3s`: a continue token works, then is a 410
`Expired` with an inconsistent token in `metadata.continue`, which lists the
rest (including an object created after the first page) at one newer
revision; an `Exact` LIST at the old revision and a WATCH from below the
compaction are 410 `Expired`; `apiserver_storage_compacted_revision` moves.

`e2e/cr-schema.sh` (#121, suite `rigs`) replays the conformance suite's
FieldValidation and CRD-defaulting bodies over HTTP: Strict server-side apply
refused for an undeclared field, unknown root/embedded metadata and a
repeated YAML key, with upstream's messages; a valid CR created; without
Strict the CR created, pruned and warned about; a Strict JSON create with a
repeated key refused; a default filled on create, and a default added to the
CRD later shown on GET and LIST.

Every rig that starts kube-controller-manager or kube-scheduler (through
`lib.sh`, `scheduler-failover.sh`, `multi-master.sh`) runs them as
`system:kube-controller-manager` / `system:kube-scheduler`, held to their
bootstrap ClusterRoles (#176), so a missing permission fails a rig;
`RK_CONTROL_PLANE_ADMIN=1` runs them as `system:masters` to compare.

`e2e/pod-limit.sh` holds the scheduler to a node's `allocatable.pods`
(#194): on a 2-pod stand-in Node, 3 BestEffort Pods bind 2 and the third
reports `PodScheduled=False/Unschedulable` "0/1 nodes are available: 1 Too
many pods." until one bound Pod is Succeeded; a burst of 30 onto a 5-pod Node
binds exactly 5, and deleting one lets exactly one more bind.

`e2e/scheduler-failover.sh` runs two electing schedulers: the leader is
paused past its lease, the standby takes over, the resumed old leader binds
nothing, the standby is killed and the old leader takes over again — a
one-CPU stand-in Node is never overcommitted. It then checks that a missing
claim's arrival, and a claim's binding after selected-node, wake the waiting
Pod onto its volume's node (#145).

`e2e/deadlines.sh` holds each controller deadline to its moment (CronJob
start, Job `activeDeadlineSeconds`, Event TTL, Lease grace, ReplicaSet
recreation backoff), checks GC fail-closed for an unserved owner kind, and
requires an idle control plane to make no API requests for a minute — the
VirtualMachine controller's 30 s retries of an unserved KubeVirt API are
listed, not counted (#172).

`e2e/watch-deadline.sh` (#207) idles the controller-manager and scheduler
for 400 s, past the reflector's own 330 s WATCH deadline, and requires that
their watches ended and resumed (the apiserver saw new WATCHes) with no
`reflector WATCH reconnecting` warning, no LIST, and — when the fixed metrics
port 10257 is the rig's own — no change in `rustkube_watch_reconnects_total`.

`e2e/serving-cert.sh` (#93) serves the rig's openssl RSA pair from files and
checks, a reload tick (30 s) at a time: `deploy/renew-certs.sh` with a signing
failure exits 1 and changes no file; a key written without its certificate
leaves the old pair serving verified handshakes, logged once; a real renewal
(SANs kept) is served without a restart; and an apiserver started on a
mismatched pair exits with the reason. About three minutes.

`e2e/client-cert-reload.sh` (#105) runs kube-controller-manager and
kube-scheduler on x509 client certificates from client CA A, with
`--client-ca-file`, and rolls them: the CA file becomes A+B (a B certificate
is accepted without a restart); each component's pair is renewed from B key
first (the mismatch refused once, the components still working), then the
certificate (reload logged); the CA file becomes B alone (an A certificate is
refused); and the apiserver restarts, so every connection is a new handshake —
the controller-manager still turns a Deployment into a ReplicaSet and the
scheduler still binds a Pod. About four minutes.

`e2e/get-latency.sh` (#177) times GET against LIST for a ServiceAccount that
RBAC must authorize and for system:masters, idle and under 40 clients renewing
Leases; counts datastore calls per authorized GET (one); checks that a grant
applies at once and a revocation within the watch's delay; and decodes a
metadata-only CRD watch. Its idle p99 bound is `RK_GET_P99_MS` (default 50).
Under load the datastore's linearizable Range sets the pace, so compare with
`RK_RELEASE=1` (release binaries) and `RK_FASTETCD_REF=v1.9.0` or later
(fastetcd#71): on the default v1.6.1 a linearizable Range queues behind
writes, and the load numbers measure that queue.

`e2e/token-auth.sh` (#188) starts the apiserver with `--token-auth-file`
pointing at a file that does not exist yet, then writes stormpump#78's line
(`<token>,system:admin,system:admin,"system:masters"`) and checks the token is
accepted within the 5 s re-read, can write, and is `system:admin` in
`system:masters` by TokenReview; that a near-miss token is refused, the JWT
path still works and the token is never logged; that a rewrite rotates it, a
malformed rewrite keeps the last good token, and removing the file revokes it.
`RK_APISERVER_ARGS` (lib.sh) passes the extra flag.

`e2e/admission-webhook.sh` (#82) runs a Python HTTPS webhook (certificate
from the rig's CA, sent as `caBundle`) and checks: a validating webhook with
an `objectSelector` refuses a labelled ConfigMap create with upstream's
message and code and is never sent an unlabelled one; a mutating webhook
reached through a Service (fixed ClusterIP 127.0.0.7, TLS for
`wh.webhooks.svc`) with a `namespaceSelector` patches creates in its namespace
only, its warning comes back as a `Warning` header, and its review names the
user, operation, kind, resource, name and namespace; a PATCH is an UPDATE with
`oldObject`; a `nodes/status` PUT is admitted as subresource `status` and the
main resource is not; DELETE and deletecollection are refused for a protected
Secret; an unreachable webhook refuses under `failurePolicy: Fail` (500) and
is passed over under `Ignore`; a Fail webhook on webhook configurations
cannot block its own removal; and with the configurations deleted, writes
are no longer refused.

`e2e/crd-restart.sh` (#185) creates two CRDs (two served versions, cluster
scope) with a CR each, then checks that `/apis`, each group-version and the CRs
are served after an apiserver restart, after a "reboot" (datastore and
apiserver both stopped, the apiserver started 15 s before the datastore), and
by a second apiserver on the same store, which also must serve a CRD created
through the other and drop one deleted through it.

`bash test/e2e/list-snapshot-race.sh` checks that concurrent LIST contents
and resourceVersion agree with acknowledged writes, including pinned continuation
pages and exact WATCH replay after distinct early, middle and late snapshots. It runs without
controllers, isolating the datastore/API contract required by informer caches.

Without a prebuilt `RK_FASTETCD` (the test image has one), the rig builds
fastetcd at `RK_FASTETCD_REF`, default in `test/e2e/versions.sh` (v1.12.0;
anything from v1.6.1 has the Range snapshot fix, fastetcd#50), and prints its
source commit. The pin is a test dependency, not a deployed datastore
upgrade.

The DaemonSet heartbeat regression first waits for both Pod placement and
status accounting to converge. It then checks that heartbeat-only updates
leave the DaemonSet revision and placement unchanged; failures print both
DaemonSet observations.

Disposable API rigs require Linux `/proc/sys/net/ipv4/ip_local_port_range`
to keep listener candidates outside outbound ephemeral ports. They select
and check ports after compilation, immediately before starting servers.
`RK_PORT_OFFSET` remains an explicit port override.
