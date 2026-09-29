# Turbomode handoff — 2026-09-29

The owner switched work back to stormcentral. This is unfinished engineering,
not a validated release. All implementation and harness changes are committed
and pushed on branches named `turbomode`; no live scale test has run.

## Projects and saved work

| Project | Implementation head | Scope and remaining work |
|---|---|---|
| rustkube | `f02e2d7` | Revisioned watches, coalescing queues, cache notifications, owner-indexed workers for five controllers, consistent API pagination and leadership mutation deadlines. Finish remaining indexed controllers/scheduler and validation: [#142–149](https://github.com/glennswest/rustkube/issues?q=is%3Aissue+is%3Aopen+turbomode). |
| rustkube-node | `9b46886` | Runtime/filesystem/CSI/template completion wakeups, failed-LIST protection, teardown retry preservation, draft common Pod/VMI executor and reservations. Wire the common runtime adapters and finish event sources/cleanup safety: [#99–102](https://github.com/glennswest/rustkube-node/issues?q=is%3Aissue+is%3Aopen+turbomode). |
| stormcos_qa | `d8f3c92` | Opt-in 1,000-Pod sleep and 100-PVC SQLite tests, allocation audit and harness regressions. Add requested bounded retries and execute validation: [#26](https://github.com/glennswest/stormcos_qa/issues/26). |
| stormcentral | No turbomode code change | Build-host access blocker [#170](https://github.com/glennswest/stormcentral/issues/170); existing release performance reporting supplies the baseline. |

Stormpump, stormblock, stormvm and stormcos are integration dependencies. No
turbomode changes were made to those repositories. Deployment/version pins and
real-release verification remain future integration work; do not infer a shipped
release from these branch commits.

## Resume order

1. Resolve stormcentral#170. Builds were attempted **on dev after 10 a.m.
   America/Chicago** and stopped before compilation/test execution with
   `Host key verification failed`. `sc-build` uses `ssh -F none`, while the
   ordinary SSH configuration does not persist known hosts. The proposed
   first-use host-key registration was interrupted; recheck rather than assume
   it happened. Keep builds on dev and do not disable changed-key checks.
2. Run the committed code on dev with:

   ```sh
   # From rustkube
   sc-build 'cargo test --locked -p apimachinery -p storage -p apiserver -p controller-manager -p scheduler'
   # From rustkube-node
   sc-build 'cargo test --locked -p kubelet'
   # From stormcos_qa
   sc-build 'python3 tools/turbomode/selftest.py'
   ```

   Local syntax/format checks passed; these do not establish compilation or
   runtime correctness. Fix actual build/test failures before advancing.
3. Finish indexed controllers and scheduling under rustkube#146. The migrated
   controllers are Deployment, ReplicaSet, StatefulSet, Job and CronJob;
   whole-pass adapters remain elsewhere. Validate write overlays, cache sync,
   deletion old-state routing, resource reservations and fail-closed GC.
4. Wire node Pod and VMI adapters into **one common queue/executor/admission
   framework**, with runtime-specific operations behind adapters. The current
   common module is not wired; serialized runtime loops still execute. Seed
   reservations from adopted live workloads before admitting new work, retain
   startup barriers, and add inverse PVC/image/dependency indexes. Finish
   cancellation-safe stages and remove observation sweeps only when events or
   legitimate deadlines cover their responsibilities.
5. Complete node cleanup safety. Pod runtime records now survive failed stops;
   CSI records survive failed unstage and incomplete/unreadable observations.
   VM stop still needs equivalent fail-closed semantics: query errors and
   timeouts cannot prove exit or authorize disk release/finalizer removal.
   Audit destructive UID preconditions and shared-volume concurrency.
6. Implement bounded load-test retries in stormcos_qa#26. Keep every attempt's
   results; only retry after verified cleanup, and never hide integrity failure
   behind a later success. This request is tracked, **not implemented**.
7. Resolve test target (C2NR0Q2 or isolated dev cluster), image digests, storage
   class and node audit access. Run the two profiles sequentially. Validate
   SQLite records/checksums before and after 120-second sleep, then prove
   Kubernetes resource and backing allocation reclamation. Preserve artifacts;
   never force finalizers or manually erase backing volumes to fabricate a pass.
8. Execute the three-master failure matrix in rustkube#149. Validate cross-server
   LIST/WATCH consistency, independent leadership, paused old leaders,
   partitions, datastore quorum loss, recovery and absence of overcommit or
   erroneous cleanup. Report intentional failover delays separately from
   healthy-path p50/p95/p99/max startup latency.

All relevant issues retain their original requirements plus dated progress and
remaining-work notes. No turbomode issue was closed as completed. The provided
C2NR0Q2 OS-release baseline remains in the design documents; there are no new
subsecond, multi-master, integrity or cleanup acceptance measurements yet.
