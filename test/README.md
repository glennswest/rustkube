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

## Requirements

Nothing about the machine: no devices, sizes or node names. It needs the
node's apiserver, a scheduler with a Ready node to bind to, and (for the last
four medium checks and long's pods) a kubelet. `requires: []`.

## By hand

```bash
sc-build test/e2e/test-container.sh             # short, against a control plane on fastetcd on dev
sc-build 'test/e2e/test-container.sh short medium'
```

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
API/store rig. Run through `sc-build`; it does not need a kubelet. The shared
rig respects the build volume target and scratch paths.

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

The disposable rig builds fastetcd **v1.6.1** by default, which fixes the
Range snapshot/revision race (fastetcd#50), and prints its source commit.
`RK_FASTETCD_REF` selects another tag/branch for comparison; `RK_FASTETCD`
still accepts a prebuilt binary. All builds remain inside sc-build's private
volume. The pin is a test dependency, not a deployed datastore upgrade.

The DaemonSet heartbeat regression first waits for both Pod placement and
status accounting to converge. It then checks that heartbeat-only updates
leave the DaemonSet revision and placement unchanged; failures print both
DaemonSet observations.

Disposable API rigs require Linux `/proc/sys/net/ipv4/ip_local_port_range`
to keep listener candidates outside outbound ephemeral ports. They select
and check ports after compilation, immediately before starting servers.
`RK_PORT_OFFSET` remains an explicit port override.
