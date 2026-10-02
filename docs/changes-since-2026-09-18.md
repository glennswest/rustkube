# Source audit: changes since 2026-09-18

Checked 2026-09-29 with `git log --since=2026-09-18`, against v0.18.0 plus
`turbomode` through 0c455b8. This is a description of checked-in behavior, not
an assertion that the branch has shipped. README contains the current CLI,
ports and API summary; this page records why those descriptions changed.

## Changes and evidence

| Area | Current behavior and source | Representative commits / evidence |
|---|---|---|
| Built-in PVCs | `controller-manager/src/stormblock.rs` creates pre-bound PV objects for class `stormblock`; the kubelet clones preformatted blanks directly via stormblock, not sbregistry or CSI sidecars. WFFC waits for selected-node and records node affinity. | 0649f45, 7dfae5c; owner's description d2a0ba5; docs/storage.md |
| Placement | VMIs are scheduled; pod-level resource requests and ReadWriteOncePod affect resource/volume fit. Image-manifest inspection remains absent (#8). | b3337a0, 8362efb, d7df00e; pkg/scheduler/src/{filter,volumebinding,scheduler}.rs |
| Custom resources | Apply establishes CRDs; storage keys include API group, and bootstrap migrates old keys. Schema validation/defaulting/pruning/conversion remain absent. | 9837250, 2cc19ec; pkg/apiserver/src/crd.rs; #120/#121 |
| API identity/concurrency | Namespace defaults, PATCH CAS retries, update identity preservation, generateName, conditional status/approval PUTs and deletecollection. | 08952cb, 1fca2fd, e3b6c4e, d228904, ddd0745, 24c563f, a027b07; test/e2e/status-rv.sh |
| Projects and access | Project/ProjectRequest map to Namespaces with requester-owned RBAC; node-admin bootstrap identity; impersonation authorization. No RBAC escalation prevention or Node authorizer. | 91eccbf, eea726b, 4744c2f; handlers/project.rs, authorization.rs, server.rs; #98 |
| Wire/discovery | Per-item resourceVersions, pinned continuation revision, watch initial state and selector transitions, protobuf UID/namespace/inline fields; PriorityClass/TokenReview discovery. | 64963b2, 4939deb, 5eaab5d, a0b3ec5, 0c3e065, 72d7517, 6fd2722; storage.rs, watch.rs, discovery.rs, pkg/apimachinery/src/protobuf.rs |
| Admission/controllers | Projected SA token volume, root CA publisher, key/sysctl/immutability validation, Pending and QoS defaults; VM runStrategy and failure backoff. | c393b4b, 0683e75, c12a6ff, 289331f, 6d0ed10, c241688, f3577fb; builtin_admission.rs, rootca.rs, virtualmachine.rs |
| CSI compatibility | Upstream snapshot-controller works against CRDs/RBAC/status; expansion admission and binder preserve the external-resizer's capacity/condition handshake. These do not implement storage bytes, cluster-wide installation or node filesystem growth. | 5435b41, 0118748, 3e4f5f8, e404d72, 812df77, 69f33e3; test/e2e/snapshot-controller.sh, volume-expansion.sh |
| Component tests | `/test short|medium|long`, namespace-scoped Job permissions, API rig and controller/storage regressions. Real test-machine acceptance is still pending (#96). | 6911810, e512016, 3fe5869, bed735e; test/README.md |
| Conformance | Upstream suite has run against stand-in Nodes without kubelets; failures are triaged, not waived as passes. New binary staging broke with disposable build drives; the owner chose goldens as the route (#140), not yet implemented. | 430b268, efbea2d, c241688; docs/conformance.md; #118–139 |
| Turbomode execution | Shared indexed informer feeds, bounded per-object controller workers, a serialized priority Pod/VMI scheduler, write overlays, uncertain-create expectations, UID/RV cleanup guards and monotonic leadership deadlines. | 7f7b305, 46aa507 through af87b2d, 447faa4, 01f1a42; apimachinery/{informer,informers,workqueue}.rs, controller-manager/owned.rs; docs/event-driven-design.md |
| Delivery | Rustls uses ring; no native-certs/aws-lc requirement. Builds use sc-build private drives; Actions disabled by owner. Component goldens feed stormcos release assembly. | 6836bcb, d7bc1f8; Cargo.toml/Cargo.lock; #114; docs/releasing.md |

Paths in the table are relative to `pkg/` or the named crate's `src/` where
abbreviated. Commit IDs are local git references and can be inspected with
`git show <commit>`.

## Configuration and listener audit

All three `cmd/*/src/main.rs` CLI definitions were compared to README's
configuration tables, including explicit flag names, environment variables,
boolean syntax and defaults. No configuration-file loader or `--kubeconfig`
exists. `ApiServerConfig::default` agrees with the CLI defaults for exposed
fields. Partial SA key configuration falls back to ephemeral HS256; token
arguments take precedence over token files. `--data-dir` and `--cluster-domain`
remain accepted but unused (#88).

Apiserver serves on `0.0.0.0:6443` by default, refusing plaintext unless
`--insecure true`; controller-manager and scheduler metrics/health listeners
are fixed plain HTTP `0.0.0.0:10257` and `:10259`. No metrics endpoint requires
authentication (#90). Apiserver proxies kubelet traffic to HTTPS port 10250;
that proxy does not supply the missing kubelet streaming implementation
(rustkube-node#56).

Stormcos build configuration was separately checked at
[be718e09](https://github.com/glennswest/stormcos/blob/be718e09b5f25d1c893d9ed9a4ffd852ca3af00a/deploy/build-goldens.sh).
It now supplies controller-manager/scheduler client certificates and apiserver
client-CA verification. It still grants anonymous-admin on sno/bastion and
has no upstream CoreDNS fallback. These are build-time facts, not a live-node
inventory; no deployment was performed for this documentation audit.

## Unsupported promises and tracking

Existing issues remain the source of work, rather than filing duplicates:

| Removed or qualified claim | Tracking |
|---|---|
| Any Kubernetes client/controller works unchanged; drop-in parity | #2/#3; operation-level gaps in upstream-feature-inventory.md |
| Webhooks, aggregation, preemption, scale, gates | #82/#83/#84/#86/#87 |
| Metrics match upstream dashboards completely | #90 (shape, absent scheduling latency, incomplete attempt counts, missing queue instrumentation and auth); docs/metrics.md |
| Rotation is safe for all identities | #93/#105; issuance belongs to stormcert, #20 scope remains undecided |
| Safe LIST/informer snapshot guarantees already proved | fastetcd#50, #146; isolated test/e2e/list-snapshot-race.sh reproduced 165 inconsistent observations of 255. Since fixed in fastetcd v1.6.1 and verified by the same rig (see the 2026-10-02 addendum) |
| Storage snapshots/expansion entirely absent | API compatibility now tested (#63/#64); external/node work remains as documented in storage.md |
| Sbregistry clones built-in PVCs; engine API is loopback | #112/#117; corrected to built-in kubelet driver and per-node-token authorization |
| CI publishes current release artifacts | #114 (Actions disabled; workflow removal still pending) |
| Build output survives sc-build teardown | Newly filed #156 (legacy release script); existing #140 (conformance staging; answered "Goldens", implementation pending) |
| Terragrunt installs a current release | Newly filed #157: template requires an RPM that current delivery does not publish |
| Conformance and component test containers never existed | Historical runs and implemented suites now documented; real-node acceptance remains #96/#147/#149 |

No feature fixes, golden request, release tag, merge or deployment accompany
this refresh. Historical research pages are explicitly design input, not
implemented feature promises. Historical runbooks/results retain dates and
commits so they cannot be mistaken for verification of today's branch.

## Addendum — 2026-10-02 (commits since 2026-09-25, re-checked)

Checked with `git log --since=2026-09-25` at 53c5719. No CLI flag, default,
port or served API changed after the 2026-09-29 audit (`cmd/` and the router
are untouched). The code changes since then are in request handling and the
controller runtime:

| Area | Current behavior and source | Commits / evidence |
|---|---|---|
| RBAC read path | Bindings and roles are read from views of the watch cache, rebuilt per prefix revision; allow from memory, refuse re-checked against the datastore. One datastore read per authorized GET instead of four. GET and LIST themselves still read the datastore, `resourceVersion=0` included (#171). | 04c3e32, 6762513; apiserver/rbac_engine.rs, watch_cache.rs; test/e2e/get-latency.sh (#177) |
| Informer notifications | Heartbeat BOOKMARKs and revision-only echoes notify nobody; a routine reconnect is not a reset. | abff0aa; apimachinery/informers.rs; test/e2e/deadlines.sh (#144) |
| Controller backoff | A retained failed Pod is counted once; a cleared backoff does not requeue itself. | f58498c, 25eadee; controller-manager/{backoff,replicaset}.rs (#144) |
| Leadership | Lease renewal retries within a 10 s deadline measured from the last successful attempt's start, for scheduler and controller-manager. | 594e19d; apimachinery/lease.rs (#145) |
| Scheduler | Capacity is reserved only immediately before the bind write; a Pod waiting on a missing claim is not pinned. | d455412; scheduler/scheduler.rs; test/e2e/scheduler-failover.sh (#145) |
| Tests only | Revision-waiter tests (#148), turbomode runtime integration tests (#143). | ac7ec79, 82d75f6 |

Corrections made in this pass, from the code and the linked issues:

- fastetcd#50 is fixed (fastetcd v1.6.1); docs that still called it a blocker
  now say so (#175). fastetcd has clients besides the apiserver (stormconsole).
- stormblock-csi's StorageClass is `stormblock-csi`; the `stormblock` name
  collision is gone, and #92's provisioner check remains (#174, #175).
- Snapshot CRD/controller install moved from stormpump#28 to stormcos#170
  (open); stormblock#111 is answered — CreateSnapshot maps onto CoW snapshots.
- stormcos_qa's rustkube scripts are ported, not yet retired (stormcos_qa#25).
- #140 is answered "Goldens": conformance binaries are to come from goldens;
  not implemented yet. Persistent tests as pods on forge is #173.
- How it ships: nodes run the stormd goldens that stormcos's stage build
  compiles from a rustkube checkout; the component golden is not yet mounted
  by any release (stormcos#62). Checked against stormcos bb347bf4.
- Gaps that were already filed are now named where the docs describe the
  behavior: unbound TokenRequest tokens (#182), cluster-admin bootstrap
  bindings (#176), the VirtualMachine controller's idle retries (#172).
