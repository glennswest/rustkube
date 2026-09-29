# Control-plane scale measurement (#66)

Status, 2026-09-29: measurement plan only. No new scale run has been made.
The issue's earlier synthetic 10/100/250-node results used controller LISTs
silently truncated at 500 objects. v0.12.0 fixed pagination; those earlier
CPU numbers do not establish the cost of processing the full population.

## Execution prerequisite

The owner must select an isolated environment with an unprivileged account
and a supported way to deliver binaries built by `sc-build`. Stress workloads
run outside build slots. The old conformance staging path is unavailable
under private, disposable build volumes (#140; docs/conformance.md).
The #146 implementation now exists on turbomode, but safe-cache acceptance
is blocked on fastetcd#50; runtime scale and multi-master evidence remain
#147/#149. Do not add fake Nodes
to a shared live cluster: they could attract unrelated scheduled workloads.
No turbomode goldens or merge to main are authorized by the handoff.

## Measurement protocol

1. Pin rustkube and fastetcd commits, binary hashes, build profile, hardware,
   CPU/memory limits and datastore medium. Measure the full-pagination main
   baseline first; report turbomode separately if selected for comparison.
   Keep hardware and build profiles identical between comparisons.
2. Use fresh isolated stores at 10, 100 and 1000 synthetic Nodes, with 30
   assigned Pods per Node (300, 3000 and 30000 Pods). Create objects through
   the API, record seeding duration, and renew Node Leases throughout setup
   and measurement. These are control-plane objects with no kubelets;
   container startup and real-node capacity are outside this measurement.
3. Before sampling, verify complete paginated Node/Pod counts, unique UIDs,
   Ready Nodes and fresh Leases. Check counts and readiness throughout the
   window. Abort and retain diagnostics if eviction, missed heartbeats,
   failed pagination or population drift invalidates the sample.
4. After convergence, collect a 60-second idle window and 30 sequential
   Deployment scale trials at each size. Patch spec.replicas from 0 to 10;
   measure from accepted PATCH to observing all 10 owned Pods, resolving
   Deployment -> ReplicaSet -> Pod by UID. Start observation before the
   PATCH; record its resourceVersion and use monotonic elapsed time. Scale
   down and verify removal before the next trial. Timeouts are failed trials,
   never silently dropped. Readiness is not the completion criterion.
5. Record controller-manager CPU in cores and RSS, API request rate and
   latency histograms, and fastetcd read counters before/after each window.
   Retain raw samples, metric names, histogram boundaries and sample counts;
   report unavailable metrics explicitly. Keep observer requests identifiable
   so their load is not confused with controller traffic. Publish scale
   latency p50/p95/p99/max and failures; 30 trials give only a coarse p99.
6. Retain logs and artifacts, then remove only run-owned resources using UID
   preconditions and verify cleanup. Do not force finalizers to report success.

Publish one row per population and revision, including actual verified object
counts and sample duration. A result is a measured curve, not a universal
pass/fail capacity limit. Explain where latency or resource use becomes
unacceptable and link any follow-up defect. Unit tests cannot substitute for
these results. Indexed-controller completion remains tracked by #146.
