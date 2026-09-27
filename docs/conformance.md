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
- fetches upstream's `e2e.test` and `ginkgo` for the release matching the API
  posture the apiserver reports (1.36 — `stable-1.36.txt`), cached in
  `$HOME/target/k8s-e2e`;
- creates two **stand-in Nodes** — API objects with capacity, addresses and a
  Ready condition, kept Ready by renewing their Leases every 10 s the way a
  kubelet would;
- runs the `[Conformance]` specs 16 at a time and prints one
  `RESULT <passed|failed> <seconds> <name>` line per spec, with the failure
  message and where it failed.

### Where it runs: the conformance VM, not a build slot

A run compiles nothing: it is a test workload, and it used to hold dev's
build slots for 45–90 minutes per chunk (four at once took dev to load 48 on
2026-09-27 and stalled every project's builds). So the binaries are built
**once per commit** in one ordinary sc-build, and the suite runs on the
conformance VM, which has no toolchain:

```bash
# 1. on the agent VM, from the checkout (pushed first): build and publish
sc-build test/conformance/stage.sh       # → STAGED /build/assets/conformance/<sha>

# 2. on the conformance VM: fetch that directory, then run the chunks
S=/srv/conformance/<sha>                 # copied from dev's /build/assets/conformance/<sha>
i=0
for f in 'sig-api-machinery' 'sig-apps' 'sig-(auth|cli|instrumentation|architecture|scheduling)' \
         'sig-network' 'sig-node' 'sig-storage'; do
  i=$((i + 10))
  RK_BIN=$S/bin RK_FASTETCD=$S/fastetcd RK_PORT_OFFSET=$i RK_SUITE_TIMEOUT=45m \
    bash test/conformance/run.sh "\[$f\].*\[Conformance\]" > tmp/conf-$i.log 2>&1 &
done; wait
```

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

See the table in the closing comment of #67 and the issues it links: every
failure that is not "needs a running pod" is filed as not implemented or
implemented wrong. The first runs themselves found and fixed, in the control
plane:

- field selectors compared absent fields as missing — no node matched
  `spec.unschedulable=false`, so the suite found no schedulable node;
- no `kube-root-ca.crt` publisher — every test's namespace setup waited for it;
- a LIST could miss the caller's own just-made write (read-your-writes);
- PUT created missing objects and let a body rewrite `uid` and
  `creationTimestamp`; a patch could delete `creationTimestamp`;
- namespace termination starved default-ServiceAccount provisioning;
- `?` in a cron schedule, `/apis/` and `/apis/{group}` discovery, empty
  Service `type`, and protobuf schemas for TokenRequest/SubjectAccessReview.
