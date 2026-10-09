# CLAUDE.md — RustKube Project Instructions

## Project Overview

RustKube is a Kubernetes **control plane** in Rust — `kube-apiserver`,
`kube-controller-manager`, `kube-scheduler` — serving a subset of the wire API used by kubectl,
oc, helm and client-go; full drop-in parity is not established. Target scale: 100–1000+ nodes (largest run: a
synthetic 250 nodes, #66). **README.md describes what the code does; read it
first.** It was rewritten from the code on 2026-09-24 (#80) — keep it that way:
a behaviour change updates the README in the same commit.

**Key architectural decision:** the **kube architecture** — the API server
talks to an *external* datastore over the etcd v3 gRPC wire protocol, exactly
like upstream `kube-apiserver` → etcd. The datastore is **fastetcd**
(`../fastetcd`), a Rust, wire-compatible etcd v3 replacement.

- `storage::EtcdStore` → `KvStore` impl over the `etcd-client` crate
- Endpoints via `--etcd-servers` (required); optional mutual TLS via
  `--etcd-cacert` / `--etcd-cert` / `--etcd-key`
- No embedded store and no stormforce dependency.

Node level (kubelet) → `rustkube-node`; kube-proxy is replaced by Cilium.
Cluster DNS is stormcoredns (from a manifest); rustkube knows nothing of DNS.
In stormcos each binary is a stormd golden built from source at a pinned
commit (README, *How it ships*).

## Build & Test

On the build box only, after pushing — never on a workstation, never as root:

```bash
sc-build                              # cargo build && cargo test at the pushed commit
sc-build 'cargo test -p apiserver'    # any command
```

While dev.g8.lo is retired (stormcentral#521, 2026-10-07) plain `sc-build`
fails before it starts; `SC_BUILD_VM=1 sc-build '…'` runs the same job on a
fresh build VM and works. #521 closed, but test-run image builds and goldens
still went to dev afterwards (stormcentral#526): #101, #137, #138, #147, #149,
#150, #171, #173 and #180 wait there for their test-machine runs.

**The e2e rigs (`test/e2e/*.sh`) never run in a build slot** (#173; the
build rules: "Test workloads never hold a build slot"). They are suites of
the test image, run on a test machine:

```bash
stormcentral test run rustkube rigs --tag <machine> --commit <sha>          # day, ≤ 30 min
stormcentral test run rustkube rigs-night --tag <pve VM> --commit <sha>     # night only
```

Which rig is in which suite: `test/rigs.sh`. A new rig goes in one of its
lists. Blades are off 19:00–06:00 Chicago; pve VMs take day runs too.

`sc-build` supplies a private build volume, including HOME and TMPDIR, and
deletes it after the job. Do not override those paths or retain a checkout.
`protoc` is required (`pkg/apimachinery/build.rs`).

## Workspace Structure

```
cmd/
  kube-apiserver/            binary → apiserver
  kube-controller-manager/   binary → controller-manager
  kube-scheduler/            binary → scheduler
pkg/
  apimachinery/       errors, KvStore trait, protobuf codec, metrics, quantities, quota, selectors, cron, startup waits, TLS reload, reflector/informers/work queues, leases
  storage/            etcd v3 client (etcd-client) — keys are opaque here
  apiserver/          REST API (axum), auth, RBAC, built-in admission, watch cache, CRDs
  scheduler/          fixed filter/score functions, volume binding, VMI placement
  controller-manager/ the built-in controllers (bounded indexed object workers on turbomode)
  cloud/              EMPTY — a doc comment, no code, nothing depends on it
```

Objects are `serde_json::Value` throughout; no k8s-openapi types are used.

**Built but not wired** — modules whose docs used to read as features:
`scheduler::plugins` (unused traits),
`apimachinery::{rbac, meta}` (unused types/helpers).

## Version Locations

```
Cargo.toml → workspace.package.version
Cargo.lock → versions of all workspace packages (keep in sync)
```

## Key Dependencies

- axum 0.8, tower 0.5, hyper 1.x
- rustls 0.23 + ring (no OpenSSL — static musl binaries)
- etcd-client 0.14 (→ fastetcd); tonic 0.12 only for its error codes
- prost 0.13 + prost-reflect (client-go protobuf wire codec, from vendored `.proto`)
- jsonwebtoken 9 (ServiceAccount tokens), rcgen + x509-parser (certs)
- metrics + metrics-exporter-prometheus 0.16
- Reports the K8s 1.36 API posture (#37)

`[workspace.dependencies]` still lists crates nothing uses — kube-rs is not
among them, but hickory, nix, rtnetlink, libcontainer, oci-spec, tonic-build
and others are left from the 10-crate layout; k8s-openapi is declared and never
imported.

## Current Version: `v0.18.0`

Main carries everything since v0.18.0 (turbomode #163 and the work below)
without a version bump. Component goldens (`golden-rustkube-<digest>`) are
built from main per finished issue and named in stormcos release requests
(stormcos#366); stormcos's stage build still compiles the binaries it runs
from source (stormcos#62). Do not describe main as installed or conformant.
There is no GitHub workflow (#114); use sc-build and the component golden
path, never persistent build storage. See docs/releasing.md and
docs/changes-since-2026-09-18.md.
PVCs of class `stormblock` are the kubelet's built-in blank-clone driver;
CSI is the third-party path. Sbregistry supplies blank templates, not PVC
clone requests. README's configuration tables come from the three CLI sources.

## Work Plan

### Bootstrap tokens + forge-node CSR rules (#264, P0, stormcert#78) — IN PROGRESS 2026-10-09
- [x] `bootstrap_token.rs` (upstream's: Secret kube-system/bootstrap-token-<id>,
      type, usage, expiration, extra groups) in `SigningKeys::authenticate`
      (requests + TokenReview)
- [x] `csr_admission.rs`: spec.username/groups stamped on create, spec kept on
      update; /approval needs `approve`, /status certificate `sign` on the
      signer; bootstrappers: forge-node CSRs only, read own only (GET, watch by
      name). CM role signs its four signers. `system:storm:forge-node-signer`
      ClusterRole (reconciled, unbound: stormcos binds stormcert)
- [ ] units + `test/e2e/bootstrap-token.sh`; docs; build; golden

### --client-crl-file (#260, P2, stormcert#61) — BUILT 2026-10-09
- [x] `--client-crl-file` (repeatable, PEM or DER) → WebPkiClientVerifier
      `with_crls`, end-entity only, unknown status allowed; re-read with the
      client CA (30 s), last good kept on a bad file; units; rig
      `client-crl.sh`; README/certificates.md/CHANGELOG (6a4e0bd)
- [x] Build VM at 6a4e0bd: 620 passed / 4 ignored (apiserver 318); 0 rustc
      warnings, changed files clippy-clean
- [x] bf1bb67 DER-completeness unit; golden-rustkube-5ac87c053591
      (stormcos#424); wiring notes on stormcos#481
- [ ] client-crl rig (stormcentral#512, proposed after it); then close #260

### Docs refresh from the code (since 2026-10-02) — COMPLETE 2026-10-09
- [x] README (date, How it ships at stormcos 73091eac, rig suites, build
      volume, stale claims), every docs/ page but the research and legacy
      Terragrunt ones, test/README, CLAUDE.md, audit addendum, CHANGELOG
- [x] Filed #257 (stormblock PV reclaim from the claim), #258 (HPA not via
      `/scale`); rig day budget noted on #173. presentation.md not re-rendered

### Table-only Accept on reviews → 406 (#126, P3) — BUILT 2026-10-07
- [x] `table::{only_table, refuse_table}` on the five review routes; unit;
      `test/e2e/table-review.sh` (rigs); README/conformance/CHANGELOG
- [x] Build VM at fb08f87 exit 0 (apiserver 288);
      golden-rustkube-773e0d8ddf86 (stormcos#366)
- [ ] table-review rig after stormcentral#512; then close

### LimitRanger admission (#131, P2) — BUILT 2026-10-07
- [x] `limitranger.rs`: Container defaults + annotation, Container/Pod
      min/max/ratio, PVC storage bounds, 403 upstream wording; requests from
      limits before QoS; units; `test/e2e/limitrange.sh` (rigs); docs
- [x] Workspace at cc19063 on a build VM exit 0 (apiserver 287, 3 new);
      golden-rustkube-79c26ab4b5e5 (stormcos#366)
- [ ] `rigs` run (limitrange.sh) after stormcentral#512; then close

### stormblock PV volumeMode from the claim (#201, P2) — BUILT 2026-10-08
- [x] `stormblock::desired_pv` (PV construction out of `provision_claim`)
      sets `volumeMode` from the claim (default Filesystem); unit;
      stormblock-class.sh Block case; storage.md, CHANGELOG
- [x] Build VM sc-build-470bf3e632 at 7c859df: 612 passed / 4 ignored;
      golden-rustkube-1589ac3cd829 (stormcos#366)
- [ ] stormblock-class rig (stormcentral#512); close

### Slow pod status PUT breakdown (#191, P2) — BUILT 2026-10-08
- [x] `WritePhases` in `guaranteed_update`: read/mutate/webhooks/write/
      retry_wait histogram; `slow write` warn > 100 ms with the key;
      `slow request` warn in metrics_middleware (long-running left out);
      unit; `test/e2e/status-put-latency.sh` (rigs); docs/metrics.md, CHANGELOG
- [x] Build VM at fc74517: 611 passed / 4 ignored (#239, my unit's path,
      closed); golden-rustkube-268f9826b9cd (stormcos#366)
- [ ] status-put-latency rig (stormcentral#512); real acceptance: the next
      slow PUT on pvetest1 logs its breakdown after the release

### Secret stringData folded into data (#101, P2) — IN PROGRESS 2026-10-07
- [x] `builtin_admission::fold_string_data` on create, PUT, PATCH (before
      the immutability check, again after webhooks), startup manifests; boot
      backfill for stored Secrets; unit; README/CHANGELOG
- [x] b677499 on a build VM (`SC_BUILD_VM=1 sc-build`, job 414c3ccedc):
      workspace 534 passed / 4 ignored; `test/e2e/secret-stringdata.sh`
- [ ] rigs run + golden: test images still build on dev (c47ec57c27
      errored there) — stormcentral#521

### autoscaling/v1 served (#123, P3) — BUILT + golden, rig BLOCKED on stormcentral#512 2026-10-07
- [x] `handlers/hpa_v1.rs` (to_v1/to_v2 with upstream's annotations; v1
      routes delegating to the v2 handlers; watch converted per line; PATCH
      on the v1 view); discovery (v2 preferred + v1), apply table; autoscaling/v1
      proto vendored (Scale's schema too); units; `test/e2e/hpa-v1.sh`; docs
- [x] Build VM cabae5a433 at 7ef6fac: 577 passed / 4 ignored (#235: my
      unit caught a null-annotations insert, fixed 7ef6fac);
      golden-rustkube-10934b6134a7 (stormcos#366)
- [ ] hpa-v1 rig (stormcentral#512); close

### Admission policies served (#119, P3) — BUILT + golden, rig BLOCKED on stormcentral#512 2026-10-07
Split: (1) serve the four resources here; (2) CEL evaluation is #234 (the
`cel` crate 0.15 handles struct literals via registered StructTypes; needs
Cargo.lock via a build job; shares the engine with #217).
- [x] routes (+ cluster /status), discovery, apply table, kinds; Warning on
      create that policies are not enforced; `test/e2e/admission-policy-api.sh`
      (rigs); README/inventory/conformance/CHANGELOG; #234 filed
- [x] Build VM 2bd668d442 at 08d5974: 575 passed / 4 ignored;
      golden-rustkube-883fe8afc72d (stormcos#366)
- [ ] admission-policy-api rig (stormcentral#512); then close #119

### YAML bodies + built-in fieldValidation (#122, P2) — BUILT + golden, rig BLOCKED on stormcentral#512 2026-10-07
- [x] protos re-vendored to release-1.36 (all additive, checked per field);
      `protobuf::unknown_fields`; `field_validation.rs` middleware (POST/PUT,
      Strict/Warn/Ignore, upstream wording); protobuf_mw: YAML→JSON, missing
      Content-Type = JSON, Strict YAML duplicates; units;
      `test/e2e/field-validation.sh` (rigs); README/conformance/CHANGELOG
- [x] Build VM 84a18836ef at a3bff57: 575 passed / 4 ignored;
      golden-rustkube-96f8267cda52 (stormcos#366)
- [ ] field-validation rig (stormcentral#512); close. Not done: dropping
      unknown fields under Warn/Ignore; PATCH; CRDs

### ServiceCIDR / IPAddress served (#134, P3) — BUILT + golden, rig BLOCKED on stormcentral#512 2026-10-07
- [x] networking/v1 proto re-vendored (release-1.36, additions only);
      routes (+ cluster /status), discovery, resources_for (networking was
      missing), kinds; bootstrap `kubernetes` ServiceCIDR Ready; protobuf unit;
      `test/e2e/service-cidr.sh` (rigs); docs/CHANGELOG
- [x] Build VM 9c265a5785 at aafcbdf: 572 passed / 4 ignored;
      golden-rustkube-44fc40d7ae0d (stormcos#366)
- [ ] service-cidr rig (stormcentral#512); close. Not done (optional in the
      issue): IPAddress per ClusterIP, allocation from extra ServiceCIDRs

### VMI migrate addedNodeSelector (#208, P2) — BUILT + golden, rig BLOCKED on stormcentral#512 2026-10-07
- [x] cf83dc4 apiserver: MigrateOptions.addedNodeSelector → migration spec
      (non-string 422); scheduler: reads the migration's selector, adds it to
      the VMI's (VMI's keys win); units; vmi-migration.sh cases; docs
- [x] Build VM 104f75011a at 1ffcc34: 571 passed / 4 ignored;
      golden-rustkube-c4cbcef1dd7d (stormcos#366)
- [ ] vmi-migration rig (in `rigs`) — stormcentral#512; then close

### TokenRequest default back to 1 h (#206, P3) — SHIPPED 2026-10-07
- [x] rustkube-node#122 (d25f05c, 0cdc142, e69287f) is in a318daf, the
      rustkube-node of published stormcos 11.88 (and every later compose)
- [x] 49ee0f0 `DEFAULT_TTL_SECS` 3600; unit; README/inventory/CHANGELOG;
      build VM 9d8d14de2d 571 passed / 4 ignored; golden-rustkube-5e640df87001
      (stormcos#366, release note posted there)

### KubeVirt controllers gated on their CRDs (#172, P2) — BUILT, golden 4f7661e44994; rig BLOCKED on stormcentral#512 2026-10-07
- [x] `runner.rs::kubevirt_gate`: VM / VMI launcher / VMI migration
      controllers start when their CRDs are Established (shared CRD feed),
      stop on delete, abort with the term. deadlines.sh: strict 0 idle +
      no KubeVirt requests. README/design/CHANGELOG
- [x] Built + units on a build VM at 6235778 (job 9ffe19b531): 571 passed /
      4 ignored; golden-rustkube-4f7661e44994 (stormcos#366)
- [ ] Rigs (deadlines in rigs-night: strict 0 idle; the KubeVirt rigs in
      rigs) — stormcentral#512; then close #172

### ServiceAccount key rotation (#223, P2) — SHIPPED (golden-rustkube-4f7661e44994) 2026-10-07
- [x] `SigningKeys::from_pem`: signing key RSA or P-256; verify against
      every public key of every `--service-account-key-file` (repeatable,
      comma-separated, multi-PEM); unit (old key verifies after rotation);
      README/certificates.md (rotation steps)/CHANGELOG
- [x] Built + units on a build VM at 90208b0 (job 50292519d5): workspace
      570 passed / 4 ignored (apiserver 280, incl. the rotation unit), exit 0
- [x] golden-rustkube-4f7661e44994 (stormcos#366); marked shipped

### Scale subresource (#86, P2) — BUILT, rig BLOCKED on stormcentral#512 2026-10-07
- [x] `handlers/scale.rs`: autoscaling/v1 Scale GET/PUT/PATCH for
      deployments/replicasets/statefulsets and CRDs with
      `subresources.scale` (`CrdDefinition.scale`); discovery entries (apps +
      CR `/scale`, `/status`); protobuf path → Scale. Units;
      `test/e2e/scale.sh` (rigs, real kubectl); docs/CHANGELOG
- [x] Built + units at 90208b0 (job 50292519d5, 570 passed / 4 ignored)
- [ ] rigs: 465e6c4490 (C2NR0Q2) and 7219e85020 (server3) at 90208b0 —
      image never listed by the registry (stormcentral#512); golden
      (stormcentral#541); close #86

### Scheduling gates (#87, P2) — BUILT, rig BLOCKED on stormcentral#512 2026-10-07
- [x] ddcc821 scheduler: gated Pod → PodScheduled=False/SchedulingGated (no
      Event), placed when the last gate goes; apiserver PUT/PATCH adding a
      gate → 422 (upstream's message). Units; `test/e2e/scheduling-gates.sh`
      (rigs); README/inventory/research/presentation/test README/CHANGELOG
- [x] Built + units at 90208b0 (job 50292519d5, 570 passed / 4 ignored)
- [ ] rigs: 465e6c4490 (C2NR0Q2, 90208b0, has scheduling-gates) image never
      listed by the registry (stormcentral#512); golden (stormcentral#541);
      close #87

### HPA (#89, P2) — C BUILT, rig BLOCKED on stormcentral#512 2026-10-07
Owner (2026-10-07, twice): the real HPA, CPU/memory from **cadvisor**;
serve `metrics.k8s.io` (pods/nodes, `kubectl top`); upstream's ratio,
tolerance, scale-down stabilisation. A (inert) shipped meanwhile.
- [x] A: `hpa.rs` inert (5415161), compiled + units at 566403b
- [x] apiserver `resource_metrics.rs`: `metrics.k8s.io/v1beta1` NodeMetrics
      / PodMetrics from each node's cadvisor (`/api/v1.3/subcontainers/`,
      InternalIP, `--cadvisor-scheme/-port/-ca-file/-token-file`), root cgroup
      for nodes, `io.kubernetes.*` labels for pods, 10 s cache; discovery
      (built-in group); view reads pod metrics
- [x] HPA: upstream calculator, behavior/stabilisation/rate limits, min/max,
      conditions, currentMetrics, 15 s resync, Pod feed; no metrics → no change
- [x] Units; `test/e2e/hpa-metrics.sh` (rigs, stub cadvisor); README,
      inventory, oc-compat, presentation, design, test README, CHANGELOG
- [x] cadvisor#3 told the labels rustkube reads
- [x] Built + units at 90208b0 (job 50292519d5): controller-manager 94
      (HPA calculator/behavior units), apiserver 280 (cadvisor summary)
- [ ] rigs run: 465e6c4490 (C2NR0Q2, 90208b0) image pushed, "registry did
      not list it" (stormcentral#512; 275 MB image, small ones pass); golden
      (stormcentral#541). On stormcos pod metrics wait for cadvisor#3

### Rejected bearer token → 401, not anonymous (#115, P2) — BUILT, rig waits for test runs 2026-10-07
- [x] `auth_middleware`: a presented Bearer token nothing accepts → 401
      Status (reason Unauthorized) whatever `--anonymous-auth`; anonymous only
      without credentials (no header, empty `Bearer `, other scheme); scheme
      any case (upstream's bearertoken.go). Units; `test/e2e/bad-token.sh`
      (rigs, `RK_ANONYMOUS_AUTH=true`); README/test README/CHANGELOG
- [x] 7bebbaf on a build VM (sc-build-e80aa11f7e): workspace 548 passed /
      4 ignored (apiserver 271), exit 0. #229 (my test closure) closed
- [ ] `rigs` run (bad-token.sh) waits for stormcentral#526; golden; close

### Metrics (#90, P2) — BUILT, rig BLOCKED on stormcentral#512 2026-10-07
- [x] Reconcile metrics recorded per object pass (`owned::run`)
- [x] Buckets (upstream's for apiserver/etcd request durations and scheduler
      e2e; DefBuckets else); `render()` types process_cpu_seconds_total counter
- [x] apiserver `/metrics` inside authn + RBAC (non-resource get);
      `system:monitoring` role + binding bootstrapped
- [x] CM/scheduler: `--tls-cert-file`/`--tls-private-key-file` (HTTPS,
      reloaded), delegated TokenReview + SAR on `/metrics` (10 s cache),
      `--authorization-always-allow-paths`; roles gain the two reviews
- [x] Standby leader gauge 0 from the start; `apiserver_storage_objects`
      from resource-wide prefixes only
- [x] Units; `test/e2e/metrics-auth.sh` (rigs); rigs scrape with the admin
      token; docs/metrics.md, README, test README, CHANGELOG (BREAKING)
- [x] stormcos#384 filed (ironprom token/https for 10257/10259, certs,
      probe scheme) — must land with the golden carrying this
- [x] Compiled + units on a build VM at f7bdbe7 (job 19f5cef027): workspace
      557 passed / 4 ignored (apimachinery 117, apiserver 272), exit 0
- [ ] `rigs` run on C2NR0Q2 at f7bdbe7: 2dcef65d8a interrupted
      (stormcentral restarted), ccc3315c35 image built + pushed, then "the
      registry did not list it" (stormcentral#512). Then golden

### Service create cost (#113, P2) — BUILT, rig waits for test runs 2026-10-07
Cause (from the code): `service_ip::allocate` walked the range from offset
1, one store create-if-absent per address, so N Services = N+1 writes per
create, and concurrent creates raced for the same lowest address.
- [x] Candidate = random free address from the watch cache's view of the
      claim keys, upper band first (upstream's static band min(max(16,
      size/16), 256) last); lost race → next. Claim keys unchanged. Unit;
      `test/e2e/service-create.sh` (rigs); README/test README/CHANGELOG
- [x] fd72fbc broke main (E0373, #230; #231 auto-filed); 566403b fixed it.
      Workspace on a build VM at 566403b (job 3cc07c33b9): 553 passed / 4
      ignored (apiserver 271, controller-manager 89), exit 0
- [ ] rigs run (stormcentral#526) for the timings, golden, close #113

### Gateway controller: own classes only (#91, P2) — SHIPPED (golden-rustkube-4f7661e44994) 2026-10-07
- [x] a4670a6 `gateway.rs`: acts only on GatewayClasses with controllerName
      `rustkube.io/gateway-controller`, their Gateways, and its own HTTPRoute
      `status.parents` entries (others kept; a route of foreign Gateways not
      written); no `status.addresses`; Gateway + listeners `Programmed=False`
      (Pending, no data plane). 3 units; README/inventory/presentation/CHANGELOG
- [x] Compiled + units at 566403b (3cc07c33b9): controller-manager 89,
      workspace 553 passed / 4 ignored
- [x] golden-rustkube-4f7661e44994 (stormcos#366); marked shipped. Whether
      the controller should exist stays #70

### API aggregation wired (#83, P2) — BUILT, rig waits for test runs 2026-10-07
Done when a registered, Available APIService's group is in discovery and its
requests are proxied (metrics-server or a stub). Consumers: metrics.k8s.io
(#89's real HPA, `kubectl top`), stormblock's storage.storm.io (comment).
- [x] 4c294b3 `aggregation.rs` rewrite: APIServices followed via the watch
      cache (1 s); proxy middleware inside authn + RBAC (ClusterIP, TLS for
      `<svc>.<ns>.svc` / caBundle, `--proxy-client-cert/key-file` reloaded,
      X-Remote-User/Group, client creds dropped, responses streamed,
      unavailable 503, upgrades 501, built-in groups never); protobuf mw
      bypassed for claimed paths; availability every 5 s (Local /
      ServiceNotFound / Passed / FailedDiscoveryCheck) written on change;
      `/apis`, `/apis/{g}` list aggregated groups
- [x] `extension-apiserver-authentication` ConfigMap (client CA,
      `--requestheader-client-ca-file`, allowed names, header names; 30 s
      re-read) + `system:auth-delegator`, reader Role
- [x] Units (aggregation 5, contract 1); `test/e2e/aggregation.sh` (rigs);
      README/inventory/presentation/oc-compat/test README/CHANGELOG
- [x] Compiled + units at 566403b (3cc07c33b9; after four cancelled
      jobs, stormcentral#535): apiserver 271, workspace 553 / 4 ignored
- [ ] rigs run (stormcentral#526), stormcos issue for proxy-client certs,
      golden

### Store compaction / continue-token expiry (#139, P2) — BUILT, rig waits for test runs 2026-10-07
- [x] fastetcd: auto-compaction off by default (`--auto-compaction-retention
      0`, revisions); space reclaim above 80% keeps 1000 revisions anyway;
      stormcos passes none of these flags — no routine compaction today, and
      an apiserver loop does not conflict (treat already-compacted as
      success). Posted on #139
- [x] `compactor.rs`: upstream's round (`compact_rev_key` version CAS via
      `KvStore::compact_claim`, compact to the last win's revision),
      `--etcd-compaction-interval` (5m0s, 0 off), gauge
      `apiserver_storage_compacted_revision`; `list_page`: compacted token →
      410 `Expired` + `metadata.continue` (bare key = current snapshot);
      `ApiError::gone` reason `Expired`. Units; `test/e2e/compaction.sh`
      (rigs, with #127's watch check); README/test README/CHANGELOG
- [x] 73589ce on a build VM (sc-build-dc10dbc8b3): workspace 547 passed /
      4 ignored (apiserver 270), exit 0
- [ ] `rigs` run (compaction.sh) waits for stormcentral#526; golden; close

### nodes/{name}/proxy (#108, P3) — BUILT 2026-10-08
- [x] `handlers/node_proxy.rs` (every method → kubelet :10250, apiserver
      token, status/content type/body streamed), RBAC path arm
      (`nodes/proxy`), discovery; units; `test/e2e/node-proxy.sh` (stub
      kubelet); docs. Kubelet `/logs/` filed rustkube-node#198
- [x] Build VM at 76f125d: 607 passed / 4 ignored (apiserver 309);
      golden-rustkube-ac7748374265 (stormcos#366)
- [ ] node-proxy rig (stormcentral#512); close (node-logs itself needs
      rustkube-node#198)

### CSR controller and external signers (#199, P2) — BUILT 2026-10-08
- [x] `--csr-external-signer-names` → `CsrController::with_external_signers`;
      sign only upstream's kubernetes.io/* signers; unit;
      `test/e2e/csr-signers.sh`; README/certificates.md
- [x] Build VM at fb741a6: 610 passed / 4 ignored (controller-manager 104);
      golden-rustkube-504ec94ee5c9 (stormcos#366)
- [ ] csr-signers rig (stormcentral#512); stormcos passes the flag when
      stormcert's csr-signer is on (stormcert's session files it)

### Remove the GitHub workflow (#114) — COMPLETE 2026-10-08
- [x] `.github/workflows/images.yml` removed (458f598); Actions confirmed
      disabled (`actions/permissions` → enabled false); releasing.md, audit
      table, plan updated; workspace on a build VM 606 passed / 4 ignored

### SelfSubjectReview (#116, P3) — BUILT 2026-10-08
- [x] handler + route + discovery; system:basic-user gains the rule and is
      reconciled at boot; unit; `test/e2e/whoami.sh` (kubectl auth whoami)
- [x] Build VM at 5c0e041: 606 passed / 4 ignored (apiserver 308);
      golden-rustkube-d85a5ba35d81 (stormcos#366)
- [ ] whoami rig (stormcentral#512); close

### Ephemeral-volume controller (#94, P2) — BUILT 2026-10-08
- [x] `ephemeral.rs` (pods primary, owned PVC children; create from the
      template; foreign claim → Warning Event, never adopted); CM role
      creates PVCs; unit; `test/e2e/ephemeral-volume.sh`; README/storage.md
- [x] Build VM at 40157d0: 605 passed / 4 ignored (controller-manager 102);
      golden-rustkube-41c5a5a6ec37 (stormcos#366)
- [ ] ephemeral-volume rig (stormcentral#512); close

### Aggregated discovery (#107, P3) — BUILT 2026-10-07
- [x] `wants_aggregated`, `to_v2_resources`, `aggregated` on `/api` + `/apis`
      from `builtin_lists` (now also events/metrics) + CRD registry; aggregated
      APIs Stale; units; oc-adm.sh `inspect` → works + curl checks; docs
- [x] Build VM at 5215b7f: 604 passed / 4 ignored (apiserver 307);
      golden-rustkube-b15d23ed5f76 (stormcos#366)
- [ ] oc-adm rig (stormcentral#512); close

### authorization.openshift.io reviews (#106, P3) — BUILT 2026-10-07
- [x] `RbacEngine::who_can`; `handlers/openshift_authorization.rs` (SAR,
      LocalSAR, RAR, LocalRAR; legacy-group mapping); routes, discovery,
      advertised table; unit; oc-adm.sh verbs → works; docs
- [x] Build VM at 046d2c3: 602 passed / 4 ignored (apiserver 305);
      golden-rustkube-f4592f74ec16 (stormcos#366)
- [ ] oc-adm rig (stormcentral#512); close

### Scheduler preemption (#84, P2) — BUILT 2026-10-07
- [x] `preemption.rs` rewritten (on_node/select/waiting_for_victims over
      ClusterState + filters, PDB-aware reprieve); `Scheduler::preempt`
      (nominate, Eviction API, Preempted Event); nominated room held in
      snapshot; bind clears nominatedNodeName; scheduler role pods/eviction;
      units; `test/e2e/scheduler-preemption.sh`; docs
- [x] Build VM at 72fc783: 601 passed / 4 ignored (scheduler 64: the old
      module's 10 tests replaced by 3 on the wired path);
      golden-rustkube-7df7dc5a5562 (stormcos#366)
- [ ] scheduler-preemption rig (stormcentral#512); close

### Unserved resources → 404 (#110, P3) — BUILT 2026-10-07
- [x] `served.rs` middleware from `discovery::advertised` (the discovery
      handlers' own lists); units; `test/e2e/unserved-resource.sh`; docs
- [x] Build VM at 999f2c0: 608 passed / 4 ignored (apiserver 304);
      golden-rustkube-dd10785be2ea (stormcos#366)
- [ ] unserved-resource rig (stormcentral#512); close

### Conformance binaries from goldens (#140, P2) — BLOCKED on stormcentral#557, 2026-10-07
Owner: "Goldens". Goldens are sealed stormblock volumes on forge; nothing
reads a golden's files from another machine (`registry/<c>:<tag>` is a
display name). Filed stormcentral#557 (`component fetch <c> --commit --out`,
sha256-checked); fastetcd has no golden and no release assets (noted there).
- [ ] After #557: `vm.sh <sha>` fetches rustkube's golden binaries (+ fastetcd
      by #557's route), `stage.sh` retired, docs/conformance.md; run the six
      chunks; #159's concurrent rerun

### GC orphan failure under concurrent chunks (#159, P3) — ANALYSED 2026-10-07
- [x] efbea2d's GC (sweep, non-snapshot `live`, unconditional delete, 30 s
      interval) has both a latency and an unsafe-deletion path for the spec;
      d69e618 (#146) removed both. Remaining gap fixed: a failed owner strip
      now fails the pass (retry with backoff). `test/e2e/gc-orphan-load.sh`
      (20 concurrent runs under churn, per-run uid/RV/owners/timings);
      docs/conformance.md
- [x] Build VM at 9bff762: 606 passed / 4 ignored; golden-rustkube-0036569fb42a
- [ ] gc-orphan-load rig (stormcentral#512); conformance rerun with
      concurrent chunks after #140 (goldens to conform.g8.lo); then close

### ResourceQuota controller + admission (#124, P2) — BUILT 2026-10-07
- [x] `apimachinery::quota` (usage, scopes, formatting); apiserver
      `quota_admission.rs` (check all, CAS charge, rollback, charged-at);
      controller-manager `resourcequota.rs` (feeds + LIST for other count/,
      5 s settle, 5 min resync); units; `test/e2e/resource-quota.sh`; docs
- [x] Build VM at 46cd67b: 606 passed / 4 ignored (apimachinery 123,
      apiserver 302, controller-manager 101); golden-rustkube-4ecdc374808d
- [ ] resource-quota rig (stormcentral#512); close

### RuntimeClass (#135, P3) — BUILT 2026-10-07
- [x] node.k8s.io/v1 served; `builtin_admission::runtime_class`; scheduler
      `pod_requests` + overhead; units; `test/e2e/runtime-class.sh`; docs
- [x] Build VM at 1db5ca5 (covers a9f3fff #158, 8ea98d6 #120, 1db5ca5 #135):
      600 passed / 4 ignored (apiserver 301, scheduler 71);
      golden-rustkube-5a7defd5f41c (stormcos#366)
- [ ] rig (stormcentral#512); close

### CRD schemas in /openapi (#120, P2) — BUILT 2026-10-07
- [x] `openapi_crd.rs` (v2 definitions, v3 index + documents), handlers take
      state; `CrdRegistry::all_versions`; register drops stale versions;
      units; `test/e2e/crd-openapi.sh` (rigs, kubectl explain); docs
- [x] Build VM at 1db5ca5 (covers a9f3fff #158, 8ea98d6 #120, 1db5ca5 #135):
      600 passed / 4 ignored (apiserver 301, scheduler 71);
      golden-rustkube-5a7defd5f41c (stormcos#366)
- [ ] rig (stormcentral#512); close

### Startup manifests through admission (#158, P3) — BUILT 2026-10-07
- [x] `apply_one`: create → admit_create / CRD on_create; Reconcile →
      `resource::builtin_update` (now shared with PUT and guaranteed_update)
      / CRD on_update; Service claims released on a failed create; node
      ports carried by port name (upstream; fixes #132's matching); Service
      defaults on updates; `test/e2e/manifest-admission.sh`; docs
- [x] Build VM at 1db5ca5 (covers a9f3fff #158, 8ea98d6 #120, 1db5ca5 #135):
      600 passed / 4 ignored (apiserver 301, scheduler 71);
      golden-rustkube-5a7defd5f41c (stormcos#366)
- [ ] rig (stormcentral#512); close

### VMI verbs pause/unpause/softreboot/freeze/unfreeze (#141, P2) — BUILT 2026-10-07
- [x] `kubevirt::vmi_verb` → kubelet `/vmVerb` (rustkube-node#94), routes,
      discovery, edit role; unit; `test/e2e/vmi-verbs.sh` (rigs, stub
      kubelet); README/CHANGELOG
- [x] Build VM at 0cf5acd: 596 passed / 4 ignored (apiserver 298);
      golden-rustkube-2ac2b650df54 (stormcos#366); rustkube-node#94 closed
- [ ] vmi-verbs rig (stormcentral#512); close. On a node: stormvm#42's
      medium suite can use the verbs once both goldens are released

### ReplicationController controller (#125, P2) — BUILT 2026-10-07
- [x] `replicaset::Kind` (RS / RC), RC instance in runner; fullyLabeledReplicas;
      apiserver RC defaults; `replicationcontrollers/scale` + discovery;
      units; `test/e2e/replication-controller.sh` (rigs); docs
- [x] Build VM at 847dd1e: 595 passed / 4 ignored (controller-manager 99);
      golden-rustkube-04a64e29a29f (stormcos#366)
- [ ] replication-controller rig (stormcentral#512); close

### build-release.sh on the private volume (#156) — COMPLETE 2026-10-07
- [x] OUT under $TMPDIR, cargo's target dir, no root/persistent-mount
      header, sha256s; docs/releasing.md
- [x] `sc-build deploy/build-release.sh` at f14da25 as stormbuild on a
      build VM: 3 static binaries, 3 tarballs, 3 podman images under
      /work/job/tmp, sha256s printed, exit 0

### NodePort allocation + type changes (#132, P2) — BUILT 2026-10-07
- [x] `node_port.rs` (claim keys, `--service-node-port-range`, Plan
      commit/abort), create (admission) + update (put_object,
      guaranteed_update) + delete + startup/manifests; create failure
      releases claims; units; `test/e2e/service-nodeport.sh` (rigs); docs
- [x] Build VM at 706a807: 592 passed / 4 ignored (apiserver 295);
      golden-rustkube-e7ee3d193c98 (stormcos#366)
- [ ] service-nodeport rig (stormcentral#512); close

### Presentation rendered and inspected (#160) — COMPLETE 2026-10-07
- [x] marp-cli 4.5.1 + Chrome 155 headless on a build VM (no browser on the
      VMs; Puppeteer fetched one into TMPDIR), PNGs decoded and read here.
      d521501: 12/12 fit, no clipping; fixed slide 7 breaks, slide 8 empty
      header, stale claims; 48cb177 re-rendered 12/12 fit. Command in the
      deck's header comment

### WATCH honours timeoutSeconds (#165) — BUILT 2026-10-07
- [x] `WatchParams::timeout` (400 on a non-integer), `WatchResponseOpts
      .timeout` on every watch path, deadline in the initial and live loops;
      units; `test/e2e/watch-timeout.sh` (rigs); README/design/CHANGELOG
- [x] Build VM at ad439d8: 589 passed / 4 ignored (apiserver 292);
      golden-rustkube-b15e040af5b3 (stormcos#366)
- [ ] watch-timeout rig (stormcentral#512); close

### EndpointSliceMirroring (#133, P3) — BUILT 2026-10-07
- [x] `endpointslicemirroring.rs` (desired slices) + Service controller:
      Endpoints-by-name and mirrored-slice routes, mirror/remove, delete on
      Service deletion; own Endpoints labelled skip-mirror; units;
      `test/e2e/endpointslice-mirroring.sh` (rigs); docs
- [x] Build VM at 44b5ced: 587 passed / 4 ignored (controller-manager 98);
      golden-rustkube-fb045ca519eb (stormcos#366)
- [ ] endpointslice-mirroring rig (stormcentral#512); close

### In-place pod resize, apiserver half (#136, P2) — BUILT 2026-10-07
- [x] `handlers/pod_resize.rs` (merge resources/resizePolicy; upstream's
      ValidatePodResize incl. the version-skew guard), route, discovery;
      `qos_of`; units; `test/e2e/pod-resize.sh` (rigs); docs. Kubelet half
      filed rustkube-node#192
- [x] Build VM at 8ccfc7f: 584 passed / 4 ignored (apiserver 290);
      golden-rustkube-e4fb2ff2f004 (stormcos#366)
- [ ] pod-resize rig (stormcentral#512); close when the apiserver half is
      verified (the conformance specs need rustkube-node#192 too)

### Legacy Terragrunt/RPM path (#157) — COMPLETE 2026-10-09
Owner (2026-10-09): A, retire. Delete deploy/terragrunt, deploy/packaging,
deploy/systemd (only the RPM used it), new-tcluster.sh, replace-master.sh,
ha-soak-test.sh, verify-cluster.sh; docs/terragrunt-deploy.md → short note.
Keep gen-pki.sh / gen-node-token.sh / renew-certs.sh (certificates.md, the
serving-cert rig); gen-pki.sh's default output leaves the terragrunt tree.
- [x] 683f812: deleted; docs (terragrunt-deploy note, releasing, audit,
      CHANGELOG), .gitignore; renew-certs.sh's stale restart advice (#105)
- [x] Build VM sc-build-03a0ebb50a at 683f812 exit 0; gen-pki.sh +
      renew-certs.sh (FORCE) run clean; #157 closed. No golden (no code)

### flowcontrol.apiserver.k8s.io/v1 (#118, P3) — BUILT 2026-10-07
- [x] Routes (+ cluster /status), discovery (/apis + APIResourceList),
      resources_for, kinds; flowcontrol/v1 proto vendored (release-1.36);
      bootstrap exempt/catch-all; protobuf unit; `test/e2e/flowcontrol.sh`
      (rigs); README/inventory/conformance/CHANGELOG. Not enforced
- [x] Build VM at 451d783: 582 passed / 4 ignored (apimachinery 120);
      golden-rustkube-f52d8fa2f47f (stormcos#366)
- [ ] flowcontrol rig after stormcentral#512; then close

### CR schema defaulting/pruning/fieldValidation (#121, P2) — BUILT, rig waits for test runs 2026-10-07
- [x] 3ae6a5c `schema.rs` (defaults, prune, unknown paths; preserve-unknown,
      additionalProperties, embedded-resource, ObjectMeta), CrdDefinition
      .schema, Strategy::Custom{schema, validation}; create/PUT/PATCH/apply
      prune+default, Strict 400 (both upstream spellings) / Warn headers;
      GET/LIST default on read; Strict JSON + YAML duplicate keys.
      534c63f (migrate caller), b9a59d0 (own YAML duplicate scan)
- [x] Workspace on a build VM at b9a59d0: 541 passed / 4 ignored (apiserver
      264). #226, #227 (my intermediate failures) closed
- [x] `test/e2e/cr-schema.sh` (rigs, the conformance bodies); docs
- [ ] rigs run — test images still build on dev (stormcentral#526); golden

### Service ports in strategic merge patch (#150, P2) — WRITTEN, not yet built 2026-10-07
- [x] `ports` keyed by content: `containerPort` when entries carry it, else
      port + protocol (default TCP); `merge_id` used by merge, `$patch:
      delete` and `$setElementOrder`; unit test; README/CHANGELOG
- [ ] COMPILED + unit tests pass on a build VM at b677499; rigs + golden wait for (stormcentral#521); golden;
      close #150

### Multi-master safety and failover (#149, P2) — rig written, not run 2026-10-07
Real three-master hardware: none (owner on #162). The matrix that fits one
host is a rig: `test/e2e/multi-master.sh` (rigs-night) with lib.sh
`RK_ETCD_MEMBERS=3` / `RK_APISERVERS=3`. Existing coverage: scheduler
leader pause/kill (scheduler-failover), second apiserver (crd-restart),
paged snapshots (list-snapshot-race).
- [x] lib.sh multi-member/multi-apiserver; multi-master.sh; docs
- [ ] Run it (rigs-night, a pve VM at night): no build host until
      stormcentral#521. Then fix what it finds; real hardware stays #162's

### Least-privilege bootstrap RBAC (#176, P2) — BUILT, rigs wait for test runs 2026-10-07
Owner (2026-10-07): A now — one controller-manager role written from real
calls; C — the Node authorizer — as its own issue. Scheduler: upstream's
role + KubeVirt status writes.
- [x] c5978ee `control_plane_rbac.rs`: `system:kube-controller-manager` (read/patch/
      update/delete everything for GC + namespace deletion; create only what
      controllers create; no tokens, RBAC creates, impersonate, bind,
      escalate) and `system:kube-scheduler` (explicit reads; pod PUT,
      pods/status, PVC patch, VMI/migration status, lease, events); reconciled
      at boot; bindings repointed from cluster-admin (autoupdate opt-out)
- [x] exec/attach/portforward authorized as `create` (upstream ≥1.31), else
      `get *` would be a shell in any pod
- [x] lib.sh runs CM/scheduler as their own identities — every rig checks the
      roles; units; docs; C filed as #228
- [x] 4088060 on a build VM (job sc-build-c644eb2f68): workspace 543 passed
      / 4 ignored (apiserver 266), exit 0
- [ ] The rigs (every one exercises the roles) need a test run —
      stormcentral#526; then golden, close #176

### Scheduler Events (#138, P2) — WRITTEN, not yet built 2026-10-07
PodScheduled=False/Unschedulable landed with #194; the Events were missing.
- [x] `scheduler::events`: `Scheduled` on an acknowledged bind (spawned off
      the loop), `FailedScheduling` inside `report_pod_unschedulable` after
      its message-change check; unit for the shape; pod-limit.sh checks
- [ ] COMPILED + unit tests pass on a build VM at b677499 (534 passed / 4 ignored); rigs + golden still wait — (stormcentral#521: no build host). Then workspace
      build+test, `rigs` run (pod-limit), golden, close #138

### resource.k8s.io/v1 (DRA) served (#137, P2) — IN PROGRESS 2026-10-07
Owner (#137): "Please make sure there added, … after our turbomode merge"
(merged, #163). Conformance's 4 DRA specs are CRUD only.
- [x] 6fc3cbf: routes, discovery (+ manifest table), kinds/apiVersions,
      protobuf (`resource/v1` from release-1.34, imports all in 1.32);
      protobuf round-trip unit; discovery test
- [x] 9939bcb: `test/e2e/dra-crud.sh` (suite `rigs`); README/conformance/
      test README/CHANGELOG. Allocation filed as #225
- [ ] COMPILED + unit tests pass on a build VM at b677499 (534 passed / 4 ignored); rigs + golden still wait —: sc-build and test runs need a build host —
      stormcentral#521 (dev.g8.lo retired). Then: workspace build+test, the
      `rigs` run, golden, close #137

### #147: WFFC claims bind one at a time (PVC row) — IN PROGRESS 2026-10-07
#147's scale/multi-node acceptance waits for hardware (owner on #162: "when
we have hardware we will have it"). What the test machine measures today:
turbomode 7277704177 (pvetest1), 25 Pods each with a fresh WFFC claim:
request→scheduled p50 1.42 / p95 2.54 / max 2.58 s — over the < 1 s target,
node time excluded (plain Pods: 0.17 s p50). Cause: the PV binder's claim
controller runs `workers() = 1` (so two claims cannot choose one PV); each
bind is several writes (~100 ms), 25 in a line → 0–2.6 s.
- [x] 3d3b099 binder: 8 workers; `needs_choice` → `choosing` lock with the
      PV list read inside it; pre-bound (provisioned) PVs bind in parallel.
      Unit; workspace at 3d3b099 534 passed / 4 ignored
- [x] `test/e2e/wffc-latency.sh` (suite `rigs`); control branch
      `control/147-binder-1-worker` (ceaae45: 1 worker, rigs = wffc only)
- [x] Docs (storage.md, test/README, CHANGELOG) at 98f3624
- [ ] Test-machine runs — BLOCKED on stormcentral#521 (dev.g8.lo retired;
      the runner's build step: "No route to host", 8e8fbe4a61). Earlier:
      pvetest1 seals nothing (stormcentral#512), server3 flow-over (2 runs).
      Then: `rigs` at main + control e80e04fefd-style rerun at ceaae45,
      distributions on #147, golden. #147 proposed after stormcentral#521

### Metadata-only CRD watch decode error (#180, P2) — IN PROGRESS 2026-10-07
server1 (0.15.1) cilium agent: "unable to decode an event from the watch
stream" on its `as=PartialObjectMetadata` CRD watch; the rest of the message
(cut stream vs malformed frame) was never captured.
- [x] Code read: every metadata-only frame type is PartialObjectMetadata
      (ERROR a Status); client-go raises this error only for a real decode
      failure or a non-EOF/non-timeout stream error. stormcos manifests carry
      no non-string label/annotation values (the manifest loader stores YAML
      unchecked)
- [x] 0644141 units: every frame (ADDED, MODIFIED, DELETED with/without last
      state, BOOKMARK heartbeat/initial-end, initial ADDED, ERROR) decodes under
      Go's ObjectMeta types — pass (apiserver 255 at 0644141).
      `test/e2e/metadata-watch.sh` (suite `rigs`) written; needs a test machine
- [ ] On server1 (blade, on after 11:00 UTC): grep the cilium agent log;
      absent → close; present → the full error, reproduce

### LIST/GET from the watch cache (#171, P2) — IMPLEMENTED, awaiting a test-machine run 2026-10-07
Every LIST and GET is a linearizable fastetcd Range, `resourceVersion=0`
included; #5's cached LIST was switched off and never back on. Upstream's
cacher rules: RV "" → store; RV 0 → cache; RV N (NotOlderThan) → cache once
at N; Exact → store at N; continuation → store pinned to the token's rev.
- [x] 241ce6a (+91c82b1): `storage::Read`; `ResourceStorage::{list_page_read,
      get_read}` from the resource-wide prefix cache (`read_page`/`read_one`,
      namespace by key range), 50 ms wait else store, `{rev}:{key}` tokens,
      Exact → store at N, RV unparseable → 400; built-in + CR LIST/GET;
      `apiserver_watch_cache_reads_total`. Units: apiserver 253; workspace at
      91c82b1 527 passed / 4 ignored. #222 (my type error) closed
- [x] `test/e2e/cache-reads.sh` (suite `rigs`)
- [ ] Test-machine run of `rigs` (has cache-reads): 03a09c82aa at c82ef9e
      ended "no VM 3101" (pvetest1 lost its VM; pvetest2 too); blades off
      00–11 UTC. Rerun on a blade or a reinstalled VM; then golden, close
      #171 (server1 GET p99 < 20 ms after the release, #187)
- [ ] README/design doc/CHANGELOG; golden; close #171 (server1 GET p99 < 20
      ms acceptance after the release, per #187)

### e2e rigs as pods on the test machines (#173, P2) — IMPLEMENTED, awaiting a test-machine run 2026-10-07
Owner (#162): "Persistent tests should be pods, and should live on forge."
Every `test/e2e/*.sh` rig ran inside an sc-build slot (this session's #98,
#105, #128, #209, #210, #198 runs too). The test standard already has the
path: `test/Containerfile` (fedora-minimal + microdnf), `test/build.sh` on
the build box, extra suites declared in `test/requires.toml` (#247).
- [x] 68e3260/f3cea50: `test/build.sh` stages the commit's release (musl)
      kube-* binaries, fastetcd (`test/e2e/versions.sh`, v1.12.0), `oc`,
      `kubectl`, CSI hostpath/provisioner/resizer, snapshot-controller
      (`test/fetch-image-file.py`, registry API, no podman) and snapshot
      CRDs/RBAC; rigs prefer them via `RK_TOOLS`. sc-build `STAGE_ONLY=1`
      at f3cea50: all staged (static, oc glibc), rustkube-test 9 units pass
- [x] `/test rigs` (18 functional rigs, 1800 s) and `/test rigs-night`
      (9 slow ones, 7200 s) exec `test/rigs.sh`: JSON line per rig, logs in
      /results; `test/requires.toml` declares both
- [x] Docs: test/README, CLAUDE.md Build & Test, CHANGELOG; memory
- [ ] Runs 1591bc1258 (pvetest1) and dcb98cb338 (pvetest2) queued at
      f3cea50 since 02:4x/03:2x UTC; both VMs' runs keep erroring "node did
      not settle within 45 min", blades are off 00:00–11:00 UTC. Read the
      result (`stormcentral test show <id>`); fix what fails; rebalance the
      lists by the rigs' times; then run rigs-night at night; close #173

### CR metadata.generation (#198, P2) — COMPLETE 2026-10-06
Nothing set or bumped `generation` on custom resources, so no controller could
report `status.observedGeneration` (stormcluster#12).
- [x] d715e20 (+ 5b2ca51, a function the refactor dropped) #128's
      `StatusField` → `Strategy::{BuiltIn, Custom{status_subresource}}`: CR
      create → `generation: 1`; main update → stored (1 if none), +1 when
      `spec` changed (with the status subresource) or anything outside
      `metadata` (without); `/status`, metadata-only writes and client-sent
      values leave it. Built-ins unchanged
- [x] Unit (apiserver 251); workspace at 5b2ca51 525 passed / 4 ignored.
      `test/e2e/cr-status.sh` 50/50 at 5b2ca51 (18 generation checks);
      control (generation writes removed) fails 18. #220 (the dropped
      function) closed
- [x] README/test README/CHANGELOG
- [x] golden-rustkube-06b2476d3811 (stormcos#333); #198 closed

### Stamp the requester on storage.storm.io CRs (#210, P1) — COMPLETE 2026-10-06
stormdrive#45's controller SARs the creator of a `DriveOperation` before
touching a drive; nothing unforgeable on the object said who that was.
- [x] 212c4d6 `requester.rs` + `admission::after_mutating` (with #98's
      check): create of a `storage.storm.io` object → annotations
      `storage.storm.io/requester` (username) and `…/requester-groups`
      (groups, `,`) over the client's; every update (PUT, PATCH, apply,
      `/status`) keeps the stored values; unstamped objects stay unstamped
- [x] Units (apiserver 250); workspace at c01668c 524 passed / 4 ignored.
      `test/e2e/requester.sh` 20/20 at c01668c (fastetcd v1.12.0); control
      (stamp disabled) fails 15, every stamp check
- [x] README/test README/CHANGELOG
- [x] golden-rustkube-4b49fa1fdefc (stormcos#333); #210 closed

### VM reads Starting while the kubelet retries a failed start (#209, P1) — COMPLETE 2026-10-06
rustkube-node#76: under Always/RerunOnFailure/running the kubelet retries a
failed start itself; the VMI stays `Pending`, `reason: FailedStart`. #104's
VM status reacted only to phase `Failed`, so the VM read Starting forever.
- [x] f9de8ac `virtualmachine.rs` `failing_start`: FailedStart (not finished,
      not Running) → `CrashLoopBackOff` + `Failure` condition with the VMI's
      reason/message; `startFailure`/recreate untouched (kubelet's retry)
- [x] Unit (controller-manager 83); workspace at f9de8ac 522 passed / 4
      ignored. `test/e2e/vm-runstrategy.sh` 17/17 at f9de8ac (fastetcd
      v1.12.0, 6 new); control (`failing_start` false) fails 3 — the
      CrashLoopBackOff/condition checks, #209's symptom
- [x] README/CHANGELOG
- [x] golden-rustkube-2b0e51a10e5a (stormcos#333); #209 closed

### CR main writes overwrite status (#128, P1) — COMPLETE 2026-10-06
A CRD version with `subresources.status`: upstream's main POST drops the
body's status, main PUT/PATCH/apply keep the stored status; rustkube stored
the submitted status. CRDs without the subresource keep whole-object
semantics; `/status` writes already copied only status (+ #78's RV check).
- [x] f88a92f `CrdDefinition.status_subresource` (per version; v1beta1
      spec-level too); `StatusField::{Writable, Kept}` through `put_object`
      and `patch_stored_object` (+ apply upsert); CR create drops status
      after mutating admission. Built-ins unchanged (`Writable`)
- [x] Units: apiserver 248 (3 new); workspace at 539786c 521 passed / 4
      ignored. #219 (the new unit's missing namespace) closed
- [x] `test/e2e/cr-status.sh` 32/32 at 539786c (fastetcd v1.12.0), the
      audit's C12 for namespaced + cluster CRDs with/without the
      subresource; control (StatusField a no-op) fails 12, every isolation
      check
- [x] README/test README/CHANGELOG
- [x] golden-rustkube-2fad9faa182a (stormcos#333); #128 closed

### Client cert + client CA reload (#105, P1) — COMPLETE 2026-10-06
controller-manager/scheduler built reqwest once with `Identity::from_pem`;
the apiserver read `--client-ca-file` once. Owner (#161): certs roll
routinely, so every stormcert renewal hits this.
- [x] 0458394 `apimachinery::tls_reload`: `certified_key` (#93 key match,
      moved from apiserver), `ReloadingKey` (server + client resolver),
      `watch_files` (30 s, content); `api_client_builder` hands reqwest a
      rustls config (webpki + CA roots, insecure, reloading identity).
      CM/scheduler follow `--client-certificate/--client-key`; apiserver
      follows `--client-ca-file` (each accept takes the current config)
- [x] Units: apimachinery 113 (5 new), apiserver 245 (CA-rollover
      handshakes); workspace at 0458394 518 passed / 4 ignored
- [x] `test/e2e/client-cert-reload.sh` 17/17 at c6ddc0e (fastetcd v1.12.0);
      control (reloads disabled) fails 10, every reload-dependent check.
      serving-cert.sh 15/15 at c6ddc0e. #218 (rig's own Lease setup) closed
- [x] README/certificates.md/test README/CHANGELOG
- [x] golden-rustkube-2ff21b9b3958 (stormcos#333); #105 closed

### RBAC escalation prevention (#98, P1) — COMPLETE 2026-10-06
Any caller who may write RoleBindings could bind any role (cluster-admin in
one's own project). Upstream's rule: a binding needs `bind` on the role or
every rule of it held in the binding's scope; a role needs `escalate` or
every rule held (`aggregationRule` needs `escalate`). `system:masters`
bypasses; ownerReferences/finalizers-only updates skip.
- [x] 233fc70 `escalation.rs`: upstream `Covers` (verbs/groups/resources,
      `*/sub` and `pods/*`, resourceNames, nonResourceURLs) + `confirm`,
      called from `admission::admit` after mutating webhooks (POST, PUT,
      PATCH, apply upsert); 157c0fd Debug for RbacEngine
- [x] apiserver 246 tests (10 new); `test/e2e/projects.sh` 46/46 at 157c0fd
      (fastetcd v1.12.0); control (hook disabled) fails 8 — every escalation
      allowed. Workspace at 157c0fd 514 passed / 4 ignored, exit 0
- [x] README/inventory/presentation/CHANGELOG. Namespace writes stay
      cluster-scoped (safe to relax now; not changed)
- [x] golden-rustkube-5b99942904e4 (stormcos#306); #98 closed

### Admission webhooks wired into writes (#82, P1) — COMPLETE 2026-10-06
`admission.rs` had a webhook client nothing called; rewritten and wired.
- [x] cfb2858: request attributes in a task-local set after RBAC (user,
      verb, GVR, subresource, namespace, name, dryRun), Warning headers back;
      configs from watch-cache views; rules/scope/subresources, namespace +
      object selectors, failurePolicy, timeout, sideEffects vs dry-run
      delete, IfNeeded reinvocation, JSONPatch; url or Service (ClusterIP,
      SNI `svc.ns.svc`, caBundle). Wired: create (built-in, CR, apply upsert,
      pods/eviction), PUT, `guaranteed_update` (PATCH, /status), DELETE and
      deletecollection items. admissionregistration + events.k8s.io exempt;
      CEL matchConditions not evaluated (webhook called)
- [x] apiserver 236 tests (7 new); `test/e2e/admission-webhook.sh` 20/20 at
      54d14ba (fastetcd v1.12.0); workspace 504 passed / 4 ignored at 54d14ba
- [x] Control: rig with pkg/apiserver/src at 483a93b fails 13 of 20 (no
      webhook called); #214–#216 (control attempts) closed. CEL
      matchConditions follow-up filed #217
- [x] golden-rustkube-af640123da63 (stormcos#306); #82 closed

### 100-Pod burst: ~12 Pods wait ~60 s (#205, P1) — BLOCKED on stormcos#329, 2026-10-06
Not a scheduler defect. Run 7277704177: BestEffort `sleep 60` Pods on one
node that allows 110 Pods (rustkube-node's fixed `allocatable.pods`), with
~23 already in use. `peak_running_observed` was 87, and the rest bind as the first sleepers go
Succeeded at ~t+60 s (#194's limit, covered by `test/e2e/pod-limit.sh`).
The scheduler has no 60 s interval (backoff caps at 25.6 s).
- [x] Evidence + question posted on #205; `wait-owner`: A = configurable
      kubelet max-pods (rustkube-node + stormcos, recommended), B = QA burst
      sized/reported against free slots (stormcos_qa). No rustkube change
- [x] Owner chose A (2026-10-06): kubelet `--max-pods`, stormcos sets 250;
      stormcos_qa reports slot wait apart from scheduling. Filed
      rustkube-node#165, stormcos#329 (also: Cilium /24 ≈ 252 IPs per node);
      commented stormcos_qa#49. #205 proposed after stormcos#329
- [ ] After stormcos#329 ships: pvetest1 100-Pod burst schedules with no slot
      wait (allocatable.pods 250); then close #205

### Serving-cert reload applies a mismatched pair (#93, P1) — COMPLETE 2026-10-06
`tls.rs` reloaded any pair that parsed; `renew-certs.sh` moved the key before
the cert and kept going after a failed `openssl` (called under `||`, so no
`set -e`). Owner (#161): certs roll routinely, so every renewal hits this.
- [x] 125c277 `tls.rs`: one `certified_key` for startup and reload, refusing
      a pair whose public keys differ (`CertifiedKey::keys_match`; rustls
      0.23.37's ring keys all implement `public_key`); refusal logged once per
      change. `renew-certs.sh`: every step checked, pair compared before
      either move, `.new` files removed on failure
- [x] 3 units (P-256/P-384/Ed25519; mismatch; reload sequence); workspace at
      df35db7 500 passed / 4 ignored. `test/e2e/serving-cert.sh` 15/15 at
      70fad5e (fastetcd v1.12.0); control (be616b7 tls.rs) fails 4 — #213,
      with #211/#212 (shallow-checkout attempts), closed as control runs
- [x] golden-rustkube-ce546522ad2b (stormcos#306); #93 closed

### Reflector's own WATCH deadline is not an outage (#207, P2) — COMPLETE 2026-10-06
The apiserver ignores `timeoutSeconds` (#165), so the reflector's 330 s
reqwest timeout ended every watch: warn "reflector WATCH reconnecting",
`Unavailable` (feed unsynced, GC fails closed), reconnect counted + backoff,
recovery wake on resume — every informer, every ~5.5 min.
- [x] 4bfa056 `reflector.rs`: own deadline (tokio, not reqwest's) on an open
      stream → `Ended::Deadline`, resume from the revision at once; not
      opening by the deadline stays an outage. 2 units (200 ms deadline);
      apimachinery 108 passed; mutation (arm → Unavailable) fails the unit
- [x] `test/e2e/watch-deadline.sh` 5/5 at f0fb56a (fastetcd v1.12.0): 59
      watches resumed in 400 s idle, 0 reconnect warnings, 0 LISTs, counter
      0. Control (4bfa056 reverse-applied): cm 51 + scheduler 8 warnings,
      73 LISTs. Rig counts no `/metrics` scrapes (apiserver labels them list)
- [x] Workspace sc-build at 3a6567e: 488 passed / 4 ignored, exit 0.
      golden-rustkube-b8b7c5ee394b (stormcos#109); #207 closed

### VMI launcher Pods (#203, P1) — COMPLETE 2026-10-05
Owner's choice B on rustkube-node#88: the controller makes KubeVirt's
`virt-launcher-<vmi>-<5>` Pod for a pod-network VMI; the kubelet adopts it
(CNI identity for Cilium, status, deletion — rustkube-node 3d9d26e).
- [x] 61d980e `vmilauncher.rs`: one per VMI on `status.nodeName` (pod
      network as stormvm-spec reads it, `storm.io/bridge` wins); a migration
      target gets its own (`kubevirt.io/migrationJobUID`); the loser is deleted
- [x] 6 units; `test/e2e/vmi-launcher.sh` 24/24 at 944b89f (fastetcd v1.12.0);
      workspace 495 passed / 4 ignored; vm-runstrategy, vmi-migration,
      pod-limit 0 failed. #204 (rig's own bug) closed
- [x] Filed rustkube-node#152: kubelet's `launcher_for` must pick the Pod on
      its own node (two launchers during a migration)
- [ ] After the release + rustkube-node#88 on a node: a pod-network VM is a
      Cilium endpoint with the launcher's identity, reachable via a Service

### VirtualMachineInstanceMigration (#184, P3) — COMPLETE 2026-10-05
Control-plane half of live migration; node half rustkube-node#40. Contract is
upstream KubeVirt's VMI `status.migrationState`: controller writes
`migrationUid`/`sourceNode`/`mode`; **scheduler** writes `targetNode` (same
filters/scores as VMI placement, source excluded, charged on both nodes until
failed or moved); target kubelet writes `targetNodeAddress`; source kubelet
writes `startTimestamp`, then `completed`/`failed`/`endTimestamp`; controller
moves VMI `status.nodeName` on success.
- [x] CRD filed as stormcos#288 (YAML in the issue); rigs apply it
- [x] 8d998b4 controller `vmimigration.rs` (finalizer abort, one per VMI,
      5 min / 15 min timeouts); 18f5465 + 74d9eeb scheduler target placement
      (74d9eeb: target stays charged between `completed` and the move — the
      first rig run caught the overcommit); e33455a `migrate` subresources
- [x] Units: apimachinery 106, apiserver 229, controller-manager 76,
      scheduler 69 (21 new). `test/e2e/vmi-migration.sh` 31/31 at 74d9eeb
      (fastetcd v1.12.0); workspace 489 passed / 4 ignored at 549180c;
      vm-runstrategy 10/10, pod-limit pass, scheduler-failover 19/19.
      golden-rustkube-0fe2570f64e1 (stormcos#164). #202 (first rig run) closed
- [ ] After stormcos#288 + rustkube-node#40: a real stormvm migration
      between two nodes (the abort-while-sending path is unit-tested only)

### Pod-bound ServiceAccount tokens (#182, P2) — COMPLETE 2026-10-05
Upstream/OpenShift shape, for stormcos#54's metadata service (host-network
caller → its pod by token). Kubelet half (send boundObjectRef + 3607 s,
rotate) is rustkube-node#122.
- [x] 725dfcd TokenRequest: `spec.audiences` (default `--api-audiences`,
      default `--service-account-issuer` = https://kubernetes.default.svc),
      `expirationSeconds` 600 s–2^32 (absent stays 24 h until the kubelet
      rotates), `boundObjectRef` Pod/Secret/Node (uid 409, other SA 400);
      claims `iss`/`aud`/`nbf`/`jti` + `kubernetes.io`; pod-bound 3607 s → 1 y
      + `warnafter` (`--service-account-extend-token-expiration`)
- [x] Authentication + TokenReview: `aud`/`iss` checked when present; bound
      SA/object must exist with that uid (deleted > 60 s ago → refused), watch
      cache first, store on a miss; review reports uid + extra pod/node/jti
- [x] apiserver 227 tests (11 new); `test/e2e/bound-token.sh` 24/24 at
      214cb8f (fastetcd v1.12.0); workspace at c8a4404 468 passed / 4 ignored;
      token-auth.sh 13/13. golden-rustkube-152acbd2a1f0 (stormcos#164)
- [ ] After the release + rustkube-node#122: a pod's projected token is
      pod-bound (TokenReview names the pod), stormcos#54 can use it

### Static admin token from install-config (#188, P1) — COMPLETE 2026-10-03
storminstall writes `apiToken`; stormpump#78 writes it on first boot to
`/state/config/token-auth.csv` (0600) as `<token>,system:admin,system:admin,"system:masters"`;
stormcos#256 passes the path. Path and format accepted as stormpump wrote them.
- [x] ff60e56 `--token-auth-file` (upstream format), checked before the JWT
      path, constant-time; re-read every 5 s (late write, rotate, remove →
      revoked; malformed keeps last good); TokenReview answers it
- [x] 16b5c0b README/certificates.md/CHANGELOG; 3aed8c3 `test/e2e/token-auth.sh`
- [x] sc-build: apiserver 216 (9 new); token-auth.sh 13/13 at 3aed8c3;
      workspace at 3aed8c3 exit 0 (448 passed, 4 ignored)
- [ ] After the release + stormcos#256: `sc` logs in to a node with apiToken

### Pods beyond allocatable.pods (#194, P1) — COMPLETE 2026-10-03
pvetest1 (stormcos_qa turbomode run 4bbb76be8f): 1,000 Pods bound to a node
with `allocatable.pods = 110`. The scheduler charged CPU/memory per node but
never counted Pods, and a pod with no requests skipped resource fit entirely.
- [x] 6957abc: NodeUsage counts non-terminal Pods per node (watch, binds,
      assumptions; VMIs take no slot — rustkube-node runs no virt-launcher);
      "Too many pods" at `allocatable.pods`, checked after the placement
      filters (f1c902b)
- [x] Unplaceable Pod gets `PodScheduled=False/Unschedulable`, "0/N nodes are
      available: …", written only when it changes (Events stay #138)
- [x] At f1c902b: scheduler units 65 (3 new); `test/e2e/pod-limit.sh` 10/10;
      workspace 448 passed / 4 ignored; indexed-safety 11/11,
      scheduler-failover 19/19, schedule-latency pass (fastetcd v1.12.0).
      golden-rustkube-71422ae4df20 (stormcos#164); #194 closed. Auto-filed
      #195 was the rig's own expectation, closed
- [x] Verified on a release (#197, closed 2026-10-07): turbomode 7277704177 on
      pvetest1 — 100 Pods, peak 87 running beside the node's own, the rest
      bound as the first finished (~t+60 s); 4bbb76be8f had 1,000 bound

### Scheduler create→bind latency (#190, P1) — SHIPPED 2026-10-03
pvetest1: kubelet's `scheduled` 156→393 ms, growing pod by pod. That number is
kubelet-seen minus PodScheduled.lastTransitionTime, which the scheduler writes
truncated to whole seconds: 0–999 ms of truncation, drifting as pods are
created ~4.0x s apart. Measured, the scheduler was never the cost.
- [x] `test/e2e/schedule-latency.sh` (release, fastetcd v1.12.0). Baseline at
      3c02bd3: create→seen p50 20.7 / p99 187 ms, no growth; scheduler share
      0.2–0.8 ms; burst backlog from serialized binds; 40–45 ms watch stalls
- [x] c5b86b9 `storm.io/scheduled-at` (µs) + e2e metric emitted; 163b633 binds
      in the background (≤16, Work held, aborted with the term); 97d56d9
      apiserver TCP_NODELAY (the 40 ms stalls were Nagle + delayed ACK)
- [x] After: ADDED→bound p50 4.0–7.3 ms; scheduler share p99 ≤ 1.1 ms over 10
      runs; e2e p99 9.7–30 ms in 7 of 10 runs, the rest in the bind write's
      datastore time (etcd update ≈ 96% of the PUT) on shared dev.
      scheduler-failover 19/19 at 163b633
- [x] Workspace build+test 444 passed (4 ignored); indexed-safety 11/11 at
      5d74187; golden-rustkube-c058e790b5a4 (stormcos#164); kubelet half
      filed rustkube-node#135; #190 marked `shipped`; #192 (baseline control
      run) closed
- [ ] After the release + rustkube-node#135: pvetest1 burst of 20 by
      `storm.io/start-timing`, p50 < 20 / p99 < 50 ms

### DaemonSet pod not back after a reboot (#189, P0) — BLOCKED on stormcos#231, 2026-10-02
server3 11.65 after the power-cut reboot: cilium DS desired 1 / current 0, no pod.
- [x] Live read of server3's kube-system (23:22 UTC): the DS controller is NOT
      stuck — 32 cilium pods created since the reboot, each `Failed` within
      seconds (kubelet: the pod's own `kubernetes.io~empty-dir` /
      `~projected` dirs "do not exist" on the host), deleted, and the per-node
      failedPodsBackoff (1 s → 15 min, as upstream) leaves no pod between tries
- [x] Filed rustkube-node#129 (kubelet writes per-pod dirs in its own view,
      checks/mounts them on the host; pod should stay Pending, not Failed);
      #189 proposed after it. No rustkube code change: DS behaviour matches
      upstream's DaemonSet controller
- [x] rustkube-node#129 closed: the real cause is a full 60 MiB host root
      holding /var/lib/kubelet + /var/log/pods (ENOSPC). Kubelet now keeps such
      pods Pending with the errno (golden-rustkube-node-cb302b196e29). Host fix
      is stormcos#231 (832b627, own kubelet-data/pod-logs volumes), open until
      released; #189 proposed after it. Still no rustkube change
- [ ] After stormcos#231 ships: stormcentral's install reboot step shows
      cilium back within 60 s of node Ready; then close #189

### Stored CRDs not served after a restart (#185, P0) — SHIPPED 2026-10-02
server3 11.62 reboot: CRDs listed, but cilium.io/kubevirt.io gone from /apis and
every CR 404s. Boot registers CRDs from ONE datastore LIST whose error is
swallowed (`if let Ok`), nothing re-reads them, and /readyz is always "ok".
- [x] cc1282b: boot load pages (50) and retries a minute, logs the count;
      `follow_stored_crds` follows the CRD prefix via the watch cache (1 s
      version check; unregisters only CRDs it saw stored then gone); manifest
      Reconcile re-registers a CRD; /readyz 503 until registered
- [x] `test/e2e/crd-restart.sh` 43/43 at cc1282b (restart, reboot with store
      up 15 s after apiserver, second apiserver create/delete); apiserver 207
      unit tests incl. 2 new. Control (fix reverse-applied): fails the
      cross-replica create (404 after 20 s); restart/reboot pass on the old
      code too — the rig does not reproduce server3's failed boot read, whose
      exact cause (error vs empty answer) is unproven. Workspace sc-build
      at fc4565f exit 0
- [x] golden-rustkube-3dd6c1c7903f (stormcos#164); #185 marked `shipped`;
      install reboot check filed stormcentral#289
- [ ] After the release, server3 reboot test: cilium.io in
      /apis, cilium ready (stormcentral install reboot step)

### Docs refresh from the code (since 2026-09-25) — COMPLETE 2026-10-02
- [x] `git log --since=2026-09-25`: no CLI/port/API change since the 09-29
      audit; RBAC-from-cache (#177), informer/backoff/lease/scheduler fixes
      (#144/#145) described in README, event-driven-design, test/README
- [x] #174/#175: fastetcd#50 fixed (v1.6.1), fastetcd's other clients,
      `stormblock-csi` class, stormcos#170, stormblock#111, stormcos_qa#25,
      #140 answered "Goldens"
- [x] How it ships re-checked at stormcos bb347bf4: stormd goldens compiled
      from a rustkube checkout by stage mode; component golden unmounted
      (stormcos#62). Known gaps named in place: #171, #172, #173, #176, #182
- [x] Audit addendum in docs/changes-since-2026-09-18.md

### Single-object GET latency (#177, P0) — IN PROGRESS 2026-10-01
server1 11.57: GET of one Lease/CRD 1.3–3.7 s while lists take ms; cilium-operator
loses its lease. GET/LIST handlers and fastetcd Range are the same path, so the
suspect is per-request RBAC (LIST every ClusterRoleBinding + GET each role, all
linearizable datastore reads) for every non-system:masters client.
- [x] `test/e2e/get-latency.sh` (d1ef838): baseline — SA GET = 4 datastore
      calls (LIST CRBs + 2 role GETs + object), idle p50 11.7 ms vs admin 1.6;
      under 40-client renew load SA GET p99 1182 ms. Metadata-only CRD watch
      decodes fine (checked; not a defect on the rig)
- [x] 04c3e32: RBAC from watch-cache views (allow from memory, deny re-checks
      the store). 6762513: 1 datastore call per GET; idle SA p50 1.4 ms debug /
      0.2 ms release; grant immediate, revoke < 10 ms; apiserver 205 tests pass
- [x] Remaining load latency is the datastore, filed fastetcd#71 (P0): direct
      v3-gateway probe under the same load, linearizable Range p50 139–157 ms,
      p99 0.4–6.9 s; serializable p50 0.2–0.5 ms; fastetcd ~9% CPU
- [x] 7b36d52: rig 0 failed on release + fastetcd v1.8.0 and debug + v1.6.1;
      28da43f: workspace tests pass (4 datastore tests ignored)
- [x] golden-rustkube-0162589e3b0a (stormcos#164); #177 proposed after fastetcd#71
- [x] fastetcd#71 fixed in fastetcd v1.9.0 (bb3e828). Rig on release +
      v1.9.0: 0 failed; under load SA GET lease p99 998 → 68 ms (p50 1.1 ms),
      store linearizable p99 80 ms; idle p99 2 ms
- [x] 2026-10-01 server1 still on rustkube 0.15.1, GETs 0.4–0.9 s: neither
      golden installed yet. #177 marked `shipped` (closes with the release)
- [ ] After the release: server1 GET p99 < 50 ms, cilium 1/1, coredns Ready

### Controller deadlines (#144) — COMPLETE 2026-09-29
Poll loops were already gone from controller-manager source (remaining sleeps:
error retry, startup wait, Lease retry). The new rig found three live defects.
- [x] `test/e2e/deadlines.sh`: cron +0.0–0.1 s, Job deadline +0.1–2.7 s,
      Event TTL +0.1–1.7 s, Lease grace +0.1–1.4 s after their deadlines; RS
      backoff 10 s; GC deletes proven-absent owner's dependent, keeps an
      unserved-kind owner's; idle control plane 0 API requests in 60 s
- [x] Fixed: RS re-counted its retained Failed Pod every pass, never replaced
      it (f58498c); cleared backoff requeued itself at zero delay (25eadee)
- [x] Fixed: 45 s heartbeat BOOKMARKs reached subscribers as empty calls —
      GC/namespace re-discovered and restarted every worker, scheduler
      re-queued pending Pods; routine reconnects were resets (abff0aa)
- [x] 4024040: deadlines 9/9, indexed-safety 11, indexed-selectors 14,
      daemonset-nodes 10, scheduler-failover 19, vm-runstrategy 10, exit 0.
      5c450f5: workspace 441 passed, 4 ignored. Auto-filed #167–#170 closed.
      Live latency stays #147; no golden (#163)

### Shared watches and work queues (#143) — COMPLETE 2026-09-29
WorkQueue, reflector, reactor::WatchHub and informers::Hub were on main with
unit/fake-HTTP tests; this added the integration coverage the handoff named.
- [x] `apimachinery/src/turbomode_tests.rs` (82d75f6): 8 tests, scripted API
      server over HTTP — sharing/seed, outage (unsynced, resume from RV), 410
      and malformed-frame relists, LIST/EOF backoff, cancellation, one extra
      pass per busy pass, idle watch wakes nothing. 20/20 repeats on dev;
      five mutations (no relist, dropped dirty, no abort, outage stays synced,
      EOF hot loop) each fail a test
- [x] de33401: whole workspace 439 passed, 4 datastore tests ignored, exit 0.
      Controller poll loops are #144; live latency #147. No golden (#163)

### Watch-cache revision waiters (#148) — COMPLETE 2026-09-29
Notify-based wait, register-before-check, bounded store fallback and
termination wake were already on main (turbomode merge); this added the tests.
- [x] Wait budget as a parameter; `last_progress` on tokio time (ac7ec79)
- [x] Scripted-store tests: pump wake, re-seed wake (unchanged/changed),
      deadline fallback, pump-end wake, cancellation, 2000-round multi-thread
      stress. Mutations fail them: registering after the check hangs the
      stress; dropping pump/re-seed notifies fails three tests
- [x] c516a59: apiserver 205 passed, sc-build exit 0. No API LIST handler
      calls `WatchCache::list` (LIST reads the datastore); no latency
      attribution claimed — that is #147. No golden (#163)

### Event-driven scheduler (#145) — COMPLETE 2026-09-29
Wakeups, serialized accounting, separate renewal and term cancellation were
already on main from #146; this closed the renewal and fault-test gaps.
- [x] Lease renewal retries transient failures until 10 s after the start of
      the last successful attempt (`apimachinery::lease::hold`, scheduler +
      controller-manager). 594e19d: units 96/61/59 passed, exit 0
- [x] Found + fixed: a Pod with a missing claim kept a reservation pinning it
      to its first-choice node (d455412); unit test + rig control proves it
- [x] test/e2e/scheduler-failover.sh 19/19 on four runs (d455412, c4aec6c),
      takeover 16–18 s, no overcommit, stale leader bound nothing; the
      pre-fix control fails; indexed-safety 11/11. Live 3-master gate is #149

### #146 dev acceptance — COMPLETE 2026-09-29
- [x] Read #146, instructions and #163; continued on main. fastetcd#50 fixed
      in v1.6.1; pin API rigs to that release and report its exact commit.
- [x] Baseline 9f622fa: five-crate tests 409 passed, four storage tests ignored,
      docs passed; sc-build exit 0, 34 s. Full workspace at 226d84e: 418 passed,
      four ignored, docs passed; twenty fresh short rigs 5/5 each, exit 0, 510 s.
- [x] 52188fb: 174 complete/paginated LIST snapshots match 400 creates;
      exact WATCH suffixes at distinct early/middle/late revisions and selector
      14/14 pass; sc-build exit 0, 139 s.
- [x] Matrix: safety 11/11 and CSI expansion 14/14 at 52bbb89; DaemonSet
      10/10, VM 10/10 and short 5/5 at ec59f74 (exit 0, 206 s).
- [x] Corrected DaemonSet test status-convergence barrier (#164, closed).
      Hardened listener selection after compilation, outside ephemeral ports;
      original peer transport error #166: diagnosed 2026-10-07 as the peer-port bind failing (EADDRINUSE; tonic 0.12.3 serve exits only there), closed; fastetcd#137 to print the OS error.
- [x] Investigated #153 startup membership and #154 POST timeout. Fresh
      checks pass; original causes remain unproven. Preserve both open issues.
      #165 tracks ignored WATCH timeoutSeconds; probe uses client deadlines.
- [x] Updated README, design, handoff, test docs and changelog; #146 closure
      report records dev acceptance and retained incidents. Live validation
      stays #147/#149. No production code/version change or golden requested.

### Merge turbomode into main (#163) — 2026-09-29
Owner instruction #163 supersedes the earlier branch-only restriction below.
No golden or release is requested; unfinished acceptance stays on its issues.
- [x] Read #163 and open issues; fetched main is already in turbomode history.
- [x] Merge origin/main into turbomode: already up to date; no conflicts.
- [x] turbomode ae000d5: sc-build cargo build --workspace --locked &&
      cargo test --workspace --locked passed (51 seconds, remote exit 0).
- [x] Refreshed branch-status docs; merged with --no-ff and pushed main
      at 908d078. Same whole-workspace build/tests passed (48 seconds,
      remote exit 0). No conflicts; main was already a turbomode ancestor.
- [x] Both runs: 418 passed (apimachinery 91, apiserver 198,
      controller-manager 61, scheduler 59, test container 9); four storage
      tests ignored because they require a datastore. Doc tests passed.
      Local build-history append is read-only; remote results above passed.
- [x] Merge scope complete; closure report prepared for #163. Continue
      #142–#149 on main; #146/fastetcd#50 snapshot safety, #147 latency,
      #149 multi-master, and #153–#155 investigations remain unverified.
      Existing owner decisions #140/#161/#162 are unchanged by this merge.
Version remains v0.18.0 plus unreleased changes: this is the requested branch
integration, not completion or release of the pending turbomode feature work.

### Documentation audit — 2026-09-29
- [x] Compare history since 2026-09-18 and current source with README and every docs page.
- [x] Verify CLI defaults, listeners, API limitations, built-in stormblock PVC ownership and delivery tooling; distinguish turbomode from shipped behavior.
- [x] Track unsupported promises: existing #90/#114/#140, newly filed #156/#157; storage corrections address #112/#117.
- [x] Pushed refresh 635855d and review b906e58. All 41 CLI flags covered;
      26 relative documentation link targets resolve. sc-build at 635855d:
      five libraries 409 passed, 4 datastore-dependent tests ignored, doc tests
      passed; remote exit 0 in 33 seconds (local log append read-only).
      #112/#117 closed with documentation evidence; #114 workflow removal (done
      2026-10-08) and
      #156/#157 tooling gaps remain open. No code/version change or golden.

### History (condensed)

Phases 0–3 (v0.1.0–v0.3.0, March 2026) scaffolded a 10-crate orchestrator:
store, apiserver, scheduler, controllers, kubelet, proxy, DNS, CNI, cloud. The
kubelet/proxy/CNI moved to rustkube-node, DNS went external, the embedded
stormforce store was replaced by fastetcd, and the crates were renamed to the
upstream-shaped `cmd/` + `pkg/` layout. The Phase 0–3 checklists described
that repo and were removed on 2026-09-24 (#80): several of their items were
never wired (webhooks, aggregation, preemption) or never written (the cloud
provider). What exists now is in the README; the release history below says
when each piece landed.

### Findings from the docs pass (#80)
- [x] Admission webhooks are never called (#82) — wired 2026-10-06
- [x] Aggregation proxies nothing (#83) — wired 2026-10-07 (rig waits)
- [x] Scheduler never preempts (#84) — preemption wired 2026-10-07
- [x] `schedulingGates` honoured (#87, 2026-10-07): SchedulingGated, add
      refused; build blocked on stormcentral#544
- [x] PriorityClass / TokenReview missing from `/apis` (#85) — fixed in
      6fd2722; discovery was exercised by the efbea2d conformance run.
- [x] `/scale` subresource (#86, 2026-10-07): apps + CRDs; build blocked on
      stormcentral#544
- [x] `--data-dir`, `--cluster-domain` (#88) — BUILT 2026-10-09.
      Owner: B. `--tls` cert kept in `<data-dir>/apiserver.crt|key` (reused
      while valid, matching, > 30 d left and carrying
      `kubernetes.default.svc.<cluster-domain>`; else regenerated; unwritable
      dir → warn, in memory), files followed like `--tls-cert-file`. Units;
      rig `tls-data-dir.sh`; README/certificates.md/CHANGELOG
  - [x] 0e83609; build VM sc-build-639371ce9d: 609 passed / 4 ignored
        (apiserver 316, apimachinery 124; 6 new units)
  - [x] 8f26005 (clippy type_complexity); 0 rustc warnings, my files
        clippy-clean; workspace clippy backlog filed #263.
        golden-rustkube-d633117067d8 (stormcos#424)
  - [ ] tls-data-dir rig: 32e08fd54b (C2NR0Q2, 0e83609) "registry did not
        list it" (stormcentral#512); proposed after it; then close #88
- [x] HPA placeholder (#89) — real HPA on cadvisor metrics 2026-10-07 (rig waits)
- [x] Metrics: reconcile metrics, buckets, authenticated `/metrics` (#90)
      — 2026-10-07 (rig waits)
- [x] Gateway controller: hardcoded address, overwrites foreign classes (#91)
      — own classes only, no address, Programmed=False (2026-10-07)
- [x] Serving-cert reload applies a mismatched key/cert pair (#93) — fixed 2026-10-06
- [x] `/status` PUT is conditional on the body's `resourceVersion` (#78),
      all four handlers; `test/e2e/status-rv.sh`

### Open, found since the docs pass
- [x] GC deleted a live Deployment's ReplicaSet (#99): protobuf creates were
      stored with `uid: ""`; fixed in v0.15.3
- [x] NotFound names the store key (#109) — upstream wording + details, 2026-10-08;
      built 77d1b47 (609 passed, #238 my one-line miss), golden-rustkube-e069ee5958f3;
      rig notfound-message waits for stormcentral#512
- [x] Unserved resources answer an empty list (#110) — 404 since 2026-10-07
- [x] LIST items carry resourceVersion and continuation pages request the
      first page's revision (64963b2, #111). The still-open issue label does
      not mean this code is absent; fastetcd snapshot correctness is #50 there.
- [x] RBAC escalation prevention (#98) — 2026-10-06; Namespace writes stay
      cluster-scoped (#97)
- [x] Secrets: `stringData` folded into `data` (#101) — 2026-10-07 (rig waits)
- [x] PVC `status.phase` not defaulted to `Pending` on create (#102) — Pods,
      PVCs and PVs get `Pending` on create (#67)
- [x] No generic ephemeral-volume controller (#94) — `ephemeral.rs`, 2026-10-08
- [x] #35 closed as superseded — **owner, 2026-09-28: "tests per component,
      and only system burn or stress tests of the system in the qa."**
      rustkube's tests live in its own `test/` container, never stormcos_qa
- [ ] Test containers per the stormcos test standard (#96) — BLOCKED on stormcentral#512 (2026-10-07); IN PROGRESS
      2026-09-29: sc-build at a4dba9c compiled rustkube-test and passed all
      9 unit tests (remote exit 0; local build-log append read-only). Real
      short suite at main@d7bc1f8 was refused with HTTP 400 before any Job:
      C2NR0Q2's apiserver unavailable after three errored runs. It is the only
      registered test host; last install 11.52 failed. Blocked on
      stormcentral#63; retry the real suite after API recovery. No live pass
      and no run ID. Prior work, 2026-09-28: `test/` crate `rustkube-test` (workspace member, excluded
      from default-members), `/test short|medium|long`, JSON lines, exit
      0/1/2. Runs as stormcentral's `storm-test` SA: `*` in its namespace
      only, no cluster reads. [x] short, medium, long written (test/README.md)
      [x] test/build.sh stages a static-pie musl binary on dev
      [x] test/e2e/test-container.sh: short 5/5 on a real control plane
      [x] found + fixed: `?dryRun=All` delete deleted; workload controllers
      re-created children of an owner being deleted
      [x] rig run at 3fe5869: short 5/5, medium 14 + 1 skip (the 4
      kubelet-only checks fail on the rig, as expected), long 2 waves pass
      [ ] a real `stormcentral test run rustkube short` — 2026-10-07 run
      cf99131f71 (C2NR0Q2, 566403b): image built and pushed (the dev build
      step, stormcentral#526, is past), then "the registry did not list it"
      within 10 min — every test machine (stormcentral#512). Earlier: refused
      2026-09-28: C2NR0Q2's apiserver not answering, install 11.52 failed; the e2e scripts
      in `test/e2e/` (lib.sh + projects, status-rv, watch-deleted,
      vm-runstrategy) are the
      start of it

### Controller-manager drop-in parity (#2) — acceptance environment needed, 2026-09-29
- [x] sc-build at 5687d0f compiled kube-controller-manager and passed 58 unit
      tests plus doc tests (remote exit 0; local build-log append read-only).
      This baseline run does not verify upstream acceptance.
- [x] Read #2 and totrust#4; audit CLI, controller runner and HPA. TLS/token,
      Leases, metrics/health and workload controllers exist. Kubeconfig and
      signal handling are absent; controller presence does not establish parity.
- [x] Read totrust/PINS.yaml: the shared acceptance baseline is v1.31.4.
      This is distinct from rustkube's advertised 1.36 API posture; pin changes
      belong in totrust and must not be silently chosen in this repository.
- [ ] Owner identifies an isolated otherwise-upstream cluster with real
      kubelets and a deployment route for swapping only controller-manager.
- [ ] Complete kubeconfig, shutdown, ServiceAccount/token lifecycle,
      ResourceQuota (#124), real HPA (#89) and the controller parity review.
      Coordinate indexed workers and recovery safety with #146.
- [ ] Run all-upstream baseline and Rust-CM-only comparison: Deployment ->
      ReplicaSet -> Pods, scale up/down, owner-reference deletion and failover.
      Keep #2 open until implementation and upstream acceptance pass.

### Certificate lifecycle (#20) — scope decision needed, 2026-09-29
- [x] sc-build at 6f7e318: 3 cert-helper and 197 apiserver tests passed;
      the controller-manager csr:: filter matched no tests. Remote exit 0,
      local build-log append read-only. No renewal/rollover acceptance run.
- [x] Read #20 and inspect TLS reload, renewal tooling and current stormcert
      integration. Expiry metrics and serving reload exist; #93 still permits
      mismatched serving pairs. Client identities/trust reload remain #105.
- [x] Identify existing renewal owner: stormcert-agent has a renewal loop;
      stormcos#119 tracks wiring it into the golden. Do not duplicate it here.
- [ ] Owner reconciles #20's operator/CA-rotation roadmap with stormcert
      ownership and the ten-year CA decision recorded in stormcos#119.
      Canonical CA selection also remains open in stormcert#49.
- [ ] Once scope is settled, finish the rustkube reload obligations (#93/#105)
      and coordinate external delivery/renewal through their owning issues.
- [ ] Verify renewal without authentication loss, malformed/mismatched pair
      retention and the selected trust/key rollover contract before closure.
      cert-manager compatibility is optional and has not been implemented.

### Multi-arch image placement (#8) — acceptance target needed, 2026-09-29
- [x] Baseline sc-build at 89f36a8: apiserver 197 and scheduler 57 unit tests
      passed, doc tests passed, remote exit 0. Local build-log append was
      read-only. No image-admission or mixed-hardware acceptance was run.
- [x] Read #8; inspect built-in admission and scheduler affinity/gate handling.
      The scheduler has an arch affinity unit test. rustkube-node registration
      code sets arch/os labels; deployment on mixed hardware is not verified.
- [ ] Owner identifies an isolated mixed amd64/arm64 acceptance cluster and
      the supported deployment route for the changed control plane.
- [ ] Implement safe scheduling-gate handling (#87) before enabling gated
      placement; webhook-based admission additionally requires #82.
- [ ] Implement image resolution (indexes and single-image config), private
      pull-secret authentication, bounded credential-scoped cache, intersection
      across workload images, exclusions and affinity injection that preserves
      existing constraints. Failed inspection must not silently release a gate.
- [ ] Verify registry failure/recovery, conflicting image architectures, init
      containers, existing affinity, private secrets and excluded namespaces.
- [ ] Run real mixed-architecture placement acceptance; keep #8 open until
      admission, enforcement and runtime results are all verified.

### Scheduler drop-in parity (#3) — needs owner baseline, 2026-09-29
- [x] Baseline at 58ea6d2: sc-build compiled kube-scheduler and passed all
      57 scheduler unit tests plus doc tests (remote exit 0). Local build-log
      append was read-only. This does not verify upstream parity.
- [x] Read #3 and audit scheduler CLI, scheduling loop, score functions and
      unused plugin traits against the recorded scope.
- [x] Follow-up during #2: totrust/PINS.yaml supplies the shared acceptance
      baseline v1.31.4. README posture and research versions do not override it.
- [ ] Owner identifies an isolated otherwise-upstream acceptance cluster with
      real kubelets and a deployment route; shared pin changes belong in totrust.
- [ ] Implement kubeconfig/configuration profiles, framework/default scoring
      parity, priority/backoff/unschedulable queues and nomination, preemption
      (#84), scheduling gates (#87), and Pod scheduling events/status (#138).
      Coordinate indexed scheduling/reservations with #145/#146.
- [ ] Build/test on dev via sc-build, then compare placement against upstream
      on identical inputs, including infeasible and failure cases. Unit tests
      and the rustkube synthetic rig cannot close upstream acceptance.
- [ ] Keep #3 open until implementation and upstream acceptance are verified.

### Turbomode indexed workers (#146) — dev acceptance complete
Owner #163 authorizes integration into main; no goldens. Design: docs/event-driven-design.md;
handoff: docs/turbomode-handoff.md.

Resume checkpoint 2026-09-29: pushed fc67179 passed the handoff five-crate
`sc-build` command (exit 0; four storage integration tests ignored because
they require a running datastore). The branch already
contains the routed dependency runner and DaemonSet migration. Master update
on #146 authorizes remaining implementation now; live-target selection is
not a blocker for this work. Live validation belongs to #147/#149.
Implementation and dev acceptance are complete against fastetcd v1.6.1.
The follow-up verification above supersedes the former fastetcd#50 blocker.
Continue live acceptance in #147/#149; do not claim those gates passed.
- [x] origin/main merged into turbomode (de8c912); handoff step 2 on dev:
      `cargo test --locked` for the five crates passes (87/197/56/57, storage 4 ignored)
- [x] `owned::run` takes extra dependency feeds with routers (Node → DaemonSet,
      Pod labels → Service/PDB, PVC/PV → binder…), children optional
- [x] DaemonSet (Node eligibility + owned Pods), implemented at 826a8a4;
      API-rig node regression passed 10/10 at 72908ec
- [x] Service/EndpointSlice, PDB: selector-indexed Pod membership, UID-safe
      endpoint cleanup, CAS/no-op writes; real selector rig passed 14/14 at
      eff0752 after repairing UID-less bootstrap endpoints.
- [x] Baseline 79c1a9e: handoff five-crate command passed on dev (four
      datastore-dependent storage tests ignored).
- [x] PV binder: serialized indexed claim workers and per-PV lifecycle;
      only 404 proves claim absence; real CSI expansion passed 14/14 at 72908ec
- [x] Stormblock claim and reclaim workers; node lifecycle indexed by Lease
      name and assigned Pod, preserving expiry/toleration deadlines
- [x] Attach/detach: per-PV workers with indexed claim/Pod/driver/attachment
      dependencies and conditional detach; unit tests pass
- [x] VM (owned VMI), CSR, root CA (ConfigMap → Namespace); VM rig 10/10 at eff0752
- [x] Migration and HPA object workers with named Pod/Node/target routes;
      HPA remains the #89 placeholder, status timestamp echoes suppressed
- [x] Namespace: separate provision/teardown object pools, discovered shared
      feeds, conditional deletes and authoritative finalization confirmation
- [x] Gateway/HTTPRoute named-reference workers; stable condition timestamps
- [x] Events: per-event TTL deadlines and conditional deletion
- [x] GC: indexed resource workers fail closed on any unsynced feed; confirm
      owner absence and finalizer-dependent membership with authoritative reads
      and apply destructive UID/revision preconditions. All three propagation
      modes, Event expiry and namespace finalizers pass at 226a388
- [x] Scheduler: one serialized Pod/VMI queue, indexed storage reads, shared
      acknowledged-write accounting and retained bind/volume assumptions;
      optional VMI feed enabled by CRD observation. Shared accounting unit
      tests and burst/capacity-release API-rig checks pass at 226a388.
- [x] Five-crate final implementation tests at 9a24ce9: 409 passed, four
      datastore integration tests ignored. API safety/CSI/DaemonSet/VM checks
      pass at 1b6f951. Thirty fresh selector repetitions and ten short-suite
      repetitions pass; retain original intermittent failures #153/#154.
- [x] Audit create expectations, cache recovery, delayed acknowledgement
      history, destructive preconditions and shared resource reservations.
- [x] Isolate an upstream consistency violation: 981dcdb's
      `test/e2e/list-snapshot-race.sh` has no controllers/scheduler and detects
      165 inconsistent LIST snapshots out of 255, against 400 accepted creates.
- [x] fastetcd#50 fixed by its owning project in v1.6.1; rustkube's pinned
      snapshot/pagination/WATCH probe verifies that contract at 52188fb.
- [x] Rechecked units, startup membership and full API-rig case matrix;
      investigated #154 POST path and twenty fresh short runs. #153/#154
      remain historical incidents with no proven individual root cause.
- [x] #154 (one 30 s POST timeout in the short rig, 09-29) closed 2026-10-07
      as not reproducible: 31+ clean reruns; fastetcd#71 (the plausible
      cause) fixed in v1.9.0, rigs on v1.12.0; failures now dump diagnostics
- [x] Earlier rig failures #151 (bootstrap UID) and #152 (port/binary setup)
      fixed and closed with successful sc-build evidence. #155 records the
      original snapshot failure; post-fix acceptance evidence is recorded above.

### VM runStrategy on a failed VMI (#104) — COMPLETE 2026-09-28
- [x] Failed VMI recreated for `Always`/`RerunOnFailure`/`running: true`
      (Succeeded too for `Always`), with backoff in `status.startFailure`;
      `Once`/`Manual` leave it
- [x] printableStatus `CrashLoopBackOff` (backing off) / `Failed` (left
      failed); VMI `status.message` on a VM `Failure` condition
- [x] A refused VM status write is logged, not swallowed
- [x] `runStrategy: Once` starts the VM (it read as `spec.running`)
- [x] `test/e2e/vm-runstrategy.sh` on dev: 10/10 (v0.16.0)

### Presentation (#81) — COMPLETE 2026-09-26
- [x] `docs/presentation.md`, 12-slide Marp deck from the code as of v0.15.2;
      renders with `npx @marp-team/marp-cli docs/presentation.md` (checked on
      dev). Update it when a slide's claim changes, like any other doc

### Docs from the code (#80) — COMPLETE 2026-09-26
- [x] README, docs/, CLAUDE.md and module docs rewritten from the code
      (2026-09-24), re-checked against v0.15.2: every flag/env/default,
      ports, the 19 controllers, crate docs; stormcos/stormcert
      cross-references checked against their code
- [x] storage.md intro and CSI `CreateVolume` corrected (#103, #95)

### Watch DELETED events (#100) — COMPLETE 2026-09-26
- [x] Tombstone namespace from the key's real shape (CR keys have a group segment)
- [x] DELETED carries the object's last state from the watch cache's snapshot;
      selectors filter deletions; name-only tombstone only below the cache window
- [x] A watch from revision 0 ("from now") is served by the cache, not the store
- [x] `test/e2e/watch-deleted.sh` on dev

### Storage — current through v0.18.0
- [x] PV/PVC binding, protection finalizers, phases, reclaim, events (#56)
- [x] Attach/detach — `VolumeAttachment` for drivers that require it
- [x] Volume-aware scheduling — PV `nodeAffinity`, `selected-node`,
      `CSIStorageCapacity`
- [x] Volume expansion (#63) — COMPLETE 2026-09-28 (v0.18.0). PVC update
      validation + PersistentVolumeClaimResize, binder capacity only on
      becoming Bound, strategic-merge directives. `test/e2e/volume-expansion.sh`
      (hostpath CSI + external-provisioner v6.3.0 + external-resizer v2.2.0)
      14/14; the pre-#63 code fails 4. Node half: rustkube-node#42
- [x] Snapshots (#64) — COMPLETE 2026-09-28. Install is stormcos#170
      (moved from stormpump#28, open); CreateSnapshot maps onto CoW
      snapshots (stormblock#111, closed). `test/e2e/snapshot-controller.sh`:
      upstream v8.6.0 CRDs + the real snapshot-controller as its SA, 18/18.
      Fixed on the way: RoleBinding SA subject namespace, pre-bound claim
      binding its PV, protobuf inline embeds + webhook `Webhooks` field
- [x] `ReadWriteOncePod` enforcement — scheduler + admission (#65); the
      kubelet's mount refusal is rustkube-node#42
- [x] In-kubelet `stormblock` class: `stormblock.rs` writes the PV once the
      scheduler picks a node (#71, v0.13.0–v0.14.1)
- [x] `stormblock.rs` checks the class's provisioner (#92, 2026-10-08):
      `stormblock.storm.io` (or `/in-kubelet`); StorageClass feed wakes claims.
      Built fe60e97 (608 passed); golden-rustkube-d638c9a6eb55. Rig
      stormblock-class waits for stormcentral#512
- See [docs/storage.md](docs/storage.md) for the contract with stormblock,
  sbregistry and stormblock-csi. rustkube creates no volume bytes itself.

### Custom resource keyspace (#76) — COMPLETE 2026-09-22
- [x] #74 verified on dev (apply-created CRD serves its CRs without restart); closed
- [x] CR keys are `/registry/{group}/{plural}/...` (upstream layout); the CRD
      objects themselves and all built-ins keep their keys
- [x] Boot-time migration of old `/registry/{plural}/...` CR keys, split by the
      object's `apiVersion` group — idempotent, HA-safe, uid-checked
- [x] `handlers/kubevirt.rs` (VM/VMI) and `manifests.rs` key CRs the same way
- [x] A CRD group without a dot is refused (422) and not served
- [x] e2e on dev: seeded with the pre-#76 binary, booted the new one on the
      same fastetcd store — colliding plurals separated, uids kept, second
      boot moved nothing, same name in two groups coexists, VM `start` works

### Node ssh login (#79, part of stormcos#60)
- [x] Bootstrap `kube-system/node-admin` ServiceAccount + `node-admin`
      ClusterRoleBinding → `cluster-admin`
- [x] RS256 token signed offline with the SA signing key authenticates as
      `system:serviceaccount:kube-system:node-admin`; SA groups derived from
      `sub`, not the token. Claim contract in docs/certificates.md
- The token itself is stormcert's (stormcert#5)

### Scale measurement (#66) — awaiting execution decision, 2026-09-29
- [x] Five-crate `sc-build` at f5d3876: 400 unit tests passed, four storage
      integration tests ignored, doc tests passed; no scale workload run.
      Remote exit 0; local runs.jsonl append reported a read-only filesystem.
- [x] Read issue history: the old 250-node curve used truncated controller
      LISTs; pagination was fixed in v0.12.0, but the full-read curve is absent.
- [x] Document the 10/100/1000-node protocol in docs/scale.md, keeping fake
      Nodes alive with Leases and checking the full Pod population.
- [ ] Owner selects an isolated test environment (#162). Delivery is
      decided: goldens (#140), persistent tests as pods on forge (#173);
      neither implemented. Do not launch a stress workload before that.
- [ ] Run and publish CPU/request/latency/datastore curves and Deployment
      scale-to-Pod-creation latency; keep #66 open until measurements exist.

### Phase 4: Scale & Conformance

The dated entries below preserve the historical investigation; temporary logs
and staged artifacts are not promised to exist today. Current procedure and
results are in docs/conformance.md.

- [ ] 1000+ node testing (#66) — indexed workers now exist on turbomode,
      but their full-population resource/latency and failover curves are unverified
- [x] K8s conformance test suite (#67) — closed 2026-09-28; first run on dev,
      `test/conformance/run.sh`: e2e.test v1.36.x `[Conformance]` (443) against
      apiserver + controller-manager + scheduler with two heartbeat-kept
      stand-in Nodes and no kubelet; pod-dependent tests are expected to
      fail as "needs a node". Triage every failure into: not implemented /
      wrong / needs a real node / not applicable → issues. A full-cluster run
      needs stormcos test machines (rustkube-node#27, #32).
  - **State 2026-09-27 (session restarted mid-run):** sig-api-machinery ran
    on dev (e2e v1.36.5): 14 passed, 76 failed. Triage so far, not yet filed:
    flowcontrol (APF) not served (2); Validating/MutatingAdmissionPolicy not
    served, 3 specs hang to the suite timeout (6); aggregated discovery (#107,
    4); CRD schemas not published to /openapi (8); CR defaulting, CR
    fieldValidation unknown/duplicate, YAML request bodies refused ("Expected
    request with Content-Type: application/json") (5); CRD deletecollection
    405; CRD /status spec mismatch; autoscaling/v1 not served (Discovery
    spec); resourcequotas/podtemplates/replicationcontrollers missing from
    `resource_to_list_kind` + not in discovery (#110; ResourceQuota 10 specs,
    chunking 2); no ResourceQuota or ReplicationController controller;
    SelfSubjectAccessReview ignores Accept: Table (should 406); watch with a
    selector sends no DELETED when an object stops matching; webhook/
    conversion/aggregator specs need a pod (+ #82, #83). **Open question:**
    webhook specs fail with `resource "deployments" not found` — **found**:
    protobuf creates stored `metadata.namespace: ""`, so client-go's next GET
    had no namespace in its path and hit the CRD catch-all; fixed 2026-09-27.
  - **2026-09-27:** fixed from the api-machinery triage: empty namespace on
    protobuf create, selector watch DELETED/ADDED, deletecollection. Filed:
    APF #118, admission policies #119, CRD OpenAPI #120, CR schema #121,
    YAML/fieldValidation #122, autoscaling/v1 #123, ResourceQuota #124,
    ReplicationController #125, Table 406 #126; list kinds on #110. Not yet
    triaged: CRD /status spec mismatch (probe with RK_WHY_LINES=40),
    OrderedNamespaceDeletion, GC dependency circle — rerun first.
    Unit tests pass at 2e5dcb9 (after #129/#130, test-only). All six chunks
    + a CRD /status probe ran at 2e5dcb9 (`tmp/chunks.sh`, logs
    `tmp/conf-*.log`; the pre-fix api-machinery log is
    `tmp/conf-apimachinery-5ebe056.log`). Summaries at 2e5dcb9:
    api-machinery 15/90, misc 14/34, network 6/47, node 11/103, storage
    6/91 passed; apps still running.
  - **Fixed from that run (after 2e5dcb9, unverified by a rerun yet):** CRD
    schema x-kubernetes-* over protobuf; LIST item RVs + pinned paging +
    remainingItemCount (#111); DELETE-to-terminating retries; generateName;
    events.k8s.io protobuf + deletecollection; SA token volume injection;
    impersonation; watch without RV sends current state; EndpointSlice
    managed-by + prompt endpoints cleanup; ConfigMap/Secret key + sysctl
    validation, qosClass; immutable ConfigMap/Secret; VolumeAttributesClass.
  - **Filed from it:** LimitRanger #131, NodePort/type-change #132,
    EndpointSliceMirroring #133, ServiceCIDR/IPAddress #134, RuntimeClass
    #135, pod resize #136, DRA #137.
  - **2026-09-27, owner's rule: conformance no longer runs in build slots.**
    The last in-slot run (six chunks at 430b268, logs `tmp/conf-*.log`;
    the 2e5dcb9 run is in `tmp/run-2e5dcb9/`) is allowed to finish; no new
    ones. From ab130be: `sc-build test/conformance/stage.sh` once per commit
    → /build/assets/conformance/<sha>, then the chunks run on conform.g8.lo
    with RK_BIN/RK_FASTETCD (docs/conformance.md). conform.g8.lo did not
    resolve yet on 2026-09-27.
  - **2026-09-28:** first VM run at efbea2d: 75 passed. The drop was the
    VM's missing kubectl (fixed d1881ac; chunk rerun 19 passed, best yet);
    GC orphan was load (4/4 focused); PriorityClass `value` immutability
    fixed (c241688). docs/conformance.md has the table. #67 closed: the run
    exists and every failure is filed. **Blocked for new runs:** stage.sh
    can't write /build/assets under the no-kept-state build rule (#140).
    Owner answered **Goldens**: binaries reach conform.g8.lo as the
    component's golden; run.sh/vm.sh not yet changed. efbea2d is still
    staged there.
- [ ] ARM64 verification (#68) — PARKED 2026-10-09 until the owner's ARM64
      host is set up. aarch64 musl builds through sc-build (8e82cca:
      16M/11M/8.7M, static, start under qemu, arm64 images). Owner
      2026-10-09: measure A first (the three binaries as built; B/C from the
      numbers); "Microtik is not compatible with what we are building … no
      mikrotik, and this issue parked till hardware available." Next: on the
      host, boot apiserver against fastetcd, record RSS and startup

### `oc` compatibility — the surface that drives completeness

**[docs/oc-compatibility.md](docs/oc-compatibility.md)** is the reference: what
`oc` can ask a cluster to do, and therefore what this apiserver has to answer
for. It is the specification for "complete", not a wish list — a verb that is
not answered is a gap whether or not anyone has hit it yet.

Work it as a checklist against a live cluster rather than by reading: `oc
<verb> --help` on the target is authoritative, and the runbook's Part II is a
verification pass that should collapse into one script with an exit code.

Known state on 2026-09-24:

- [x] `oc logs` — apiserver `pods/log` proxying to the kubelet's
      `/containerLogs`, with TokenReview so anything can authenticate to a
      kubelet at all (#54). `container`, `tailLines`, `previous` work;
      `timestamps`/`sinceSeconds`/`sinceTime` are inert because stormpump logs
      are raw by design. `follow` streams end to end (rustkube-node#34).
      `limitBytes` is honored on the node. A named container may be an init or
      ephemeral one (#55).
- [ ] `oc exec`, `attach`, `port-forward` — and therefore `rsh`, `cp`, `rsync`
      and `debug`. **The apiserver half is done** (#42): proxied to the kubelet
      as a transparent connection upgrade, so SPDY and WebSocket both pass
      through; the query's `stdin/stdout/stderr` become the kubelet's
      `input/output/error`. **The kubelet half does not exist**: rustkube-node
      serves no `/exec`, `/attach` or `/portForward` (rustkube-node#56).
- [x] `oc adm` — every verb run against a live apiserver
      (`test/e2e/oc-adm.sh`); the checklist is in docs/oc-compatibility.md
      (#69). Open from it: OpenShift authorization reviews for `who-can` and
      `adm new-project` (#106, served 2026-10-07), aggregated discovery for `inspect` (#107, served 2026-10-07),
      `nodes/proxy` for `node-logs` (#108, served 2026-10-08; kubelet `/logs/` is
      rustkube-node#198); `top` needs #83
- [x] `oc scale` — `/scale` served (#86), rig `test/e2e/scale.sh`.
- [x] Projects (#97): `project.openshift.io/v1` Project + ProjectRequest over
      Namespaces, owned by their requester (`admin` RoleBinding), listed only
      to members; `admin`/`edit`/`view`/`basic-user`/`self-provisioner`
      bootstrapped. Verified by `test/e2e/projects.sh` (oc 4.22, two users).
      Namespace *writes* stay cluster-scoped until RBAC escalation
      prevention exists (#98).
- [ ] Routes, DeploymentConfig, ImageStream, BuildConfig, SCC — the
      genuinely OpenShift-only half. Whether these are in scope at all is a
      decision nobody has made (#70), and `route.openshift.io/v1` is already
      *served* with nothing routing for it, which is the inconsistency that
      forces the question; `oc` without them is `kubectl` with better
      ergonomics, which may be the right target.

## Release History

| Version | Date | Summary |
|---------|------|---------|
| v0.18.0 | 2026-09-28 | Volume expansion (#63): PVC resize validation + `PersistentVolumeClaimResize` admission; the binder leaves a Bound claim's capacity to the resize handshake. Strategic-merge directives (`$patch`, `$setElementOrder`, `$deleteFromPrimitiveList`, `$retainKeys`); `finalizers` merge as a set |
| v0.17.0 | 2026-09-28 | Test container `test/` (`/test short|medium|long`, #96). Volume snapshots work against upstream's snapshot-controller (#64): RoleBinding ServiceAccount subjects default to the binding's namespace; a claim bound by `volumeName` binds its PV; protobuf inline embeds (PV sources, volume sources, probe handlers, key-ref names) and webhook configurations' `webhooks`. `?dryRun=All` deletes no longer delete; workload controllers leave a deleting owner alone |
| v0.16.1 | 2026-09-28 | PriorityClass `value`/`preemptionPolicy` immutable; conformance `run.sh` fetches kubectl (#67) |
| v0.16.0 | 2026-09-28 | VirtualMachine honours `runStrategy` on a failed VMI: recreated with backoff under `Always`/`RerunOnFailure`, left under `Once`/`Manual`; `printableStatus` `CrashLoopBackOff`/`Failed` + `Failure` condition; `Once` starts (#104). Conformance fixes from the #67 runs: LIST item RVs (#111), SA token volumes, impersonation, generateName, Pending on create (#102), selector-watch DELETED, and more (CHANGELOG) |
| v0.15.3 | 2026-09-26 | Protobuf responses keep nested `kind`/`apiVersion` (roleRef, subjects, ownerReferences) — `oc adm policy remove-*` works. Objects created over protobuf get a real uid — the GC no longer deletes a new Deployment's ReplicaSet (#99). `oc adm` checklist (#69) |
| v0.15.2 | 2026-09-26 | Watch DELETED events: a custom resource's names its real namespace, not its plural, so informers drop it; DELETED carries the object's last state and honours selectors; a watch with no resourceVersion is served by the watch cache (#100) |
| v0.15.1 | 2026-09-26 | `PUT …/status` (and CSR `/approval`) is conditional on the body's `resourceVersion`: a stale status write is a 409 instead of silently overwriting newer status (#78). CronJob status writes chain within a pass |
| v0.15.0 | 2026-09-25 | Projects: `project.openshift.io/v1` Project + ProjectRequest over Namespaces — `oc new-project` makes the requester its admin, `oc projects` lists only one's own; `admin`/`edit`/`view` roles (#97). `oc get all` works (discovery `all` category). `kube-system/node-admin` SA for node ssh login (#79). README/docs rewritten from the code (#80) |
| v0.14.1 | 2026-09-23 | PATCH without a resourceVersion no longer 409s under concurrent writes — retried like `GuaranteedUpdate` (#77). API-created namespaces are `Active` with the `kubernetes` finalizer, backfilled at boot (#75). Resource fit honours pod-level requests (#73). Stormblock provisioner honours `WaitForFirstConsumer` |
| v0.14.0 | 2026-09-22 | Custom resources keyed by API group, `/registry/{group}/{plural}` — two CRDs sharing a plural no longer share objects; existing keys migrate at boot (#76). A CRD written by apply is registered and Established (#74). VMIs are scheduled (#72). **Breaking:** a CRD group without a dot is refused |
| v0.13.0 | 2026-09-20 | Provisioner for the in-kubelet stormblock PVC path — a claim backing a running pod no longer reads `Pending` forever (#71). `ReadWriteOncePod` enforced (#65) |
| v0.12.0 | 2026-09-09 | **Data loss fix**: controller list follows the `continue` token — past 500 objects of a kind the GC was deleting live objects whose owner fell beyond the first page (#66). `SubjectAccessReview` for `oc adm policy who-can` (#69) |
| v0.11.0 | 2026-09-09 | VirtualMachine controller + `start`/`stop`/`restart` verbs (#62). Fix: discovery paths matched by shape, so CRD groups stop 403-ing and the GC can finally see custom resources |
| v0.10.0 | 2026-09-09 | Serve `subresources.kubevirt.io/v1` console/vnc doors so `virtctl console` resolves — proxied node-ward through the kubelet, because stormvm mints only on loopback (#61) |
| v0.9.1 | 2026-09-09 | Security: bootstrap removes the `system:anonymous-admin` binding when the dev grant is off, so turning `--dev-anonymous-admin` off actually revokes it (#60) |
| v0.9.0 | 2026-09-09 | `authorization.k8s.io/v1` self-reviews so a console can ask what a user may see (#59), `system:authenticated` + `system:basic-user`. Security: a `nonResourceURLs` rule no longer granted every resource GET (anonymous could read secrets); a grouped namespaced path is no longer read as a namespace subresource |
| v0.8.1 | 2026-09-09 | `pods/log` and `pods/exec` accept an init or ephemeral container name — a failed init container's log and a shell in a sidecar were refused as "not valid for pod" (#55, #54) |
| v0.8.0 | 2026-09-08 | Storage: PV/PVC binding, attach/detach, volume-aware scheduling (#56). GC: foreground + orphan propagation, discovery-driven (#43). exec/attach/port-forward (#42) and the RBAC subresource hole they exposed. Static musl + `FROM scratch` images (#50). Upstream metric names everywhere (#51). Serving-cert hot reload + renewal (#20). Credential waiting instead of exit-1 at boot (#58) |
| v0.7.35 | 2026-07-21 | Add [profile.release] — opt3 + thin LTO + codegen-units=1, strip debuginfo, keep the symbol table so panics name functions (#49; the original entry said "keep line tables" — `strip = "debuginfo"` removes those, so backtraces give functions, not file:line) |
| v0.7.34 | 2026-07-21 | Serve events.k8s.io/v1 (translated to/from stored core/v1 Event) (#48); versioned control-plane container images CI (#46) |
| v0.7.33 | 2026-07-20 | Strategic-merge-patch honors patchMergeKey — node status.conditions upsert by type (not replaced), so nodes keep Ready after Cilium sets NetworkUnavailable (#47) |
| v0.7.32 | 2026-07-20 | Server-side apply managedFields: field-ownership tracking (fieldsV1), prune dropped fields, conflict detection (409 unless force) — completes SSA (#45) |
| v0.7.31 | 2026-07-20 | DaemonSet keys off node ELIGIBILITY not readiness (no churn on transient NotReady) + real status counts (#44); server-side apply upserts a missing object (#45) |
| v0.7.30 | 2026-07-20 | Proper DeleteOptions semantics: decode meta/v1 DeleteOptions (protobuf+JSON), honor preconditions(409)/dryRun/gracePeriod/finalizers/propagationPolicy — replaces the v0.7.27 skip |
| v0.7.29 | 2026-07-20 | CRD list/watch use the CRD real listKind (CiliumNetworkPolicyList) not {plural}List — Cilium agent CR informers sync (#39 chain) |
| v0.7.28 | 2026-07-20 | PartialObjectMetadata projection (as=PartialObjectMetadata) for list+watch — Cilium agent CRD metadata-informer syncs, goes Ready (#39 chain) |
| v0.7.27 | 2026-07-20 | protobuf mw: only transcode POST/PUT/PATCH bodies — DELETE carries DeleteOptions (no schema), was 415-ing helm/cilium uninstall |
| v0.7.26 | 2026-07-20 | JSON Patch: test-null against absent path holds (evanphx/k8s leniency) — unblocks cilium-operator node-taint CAS |
| v0.7.25 | 2026-07-20 | Watch BOOKMARK support: WatchList sendInitialEvents→initial-events-end bookmark + allowWatchBookmarks heartbeat — client-go informers sync, unblocks Cilium agent (#39) |
| v0.7.24 | 2026-07-19 | DaemonSet self-healing: delete+recreate Failed pods with random names (k8s generateName style) + failedPodsBackoff (1s→15min) — unblocks Cilium agent DS (#38) |
| v0.7.23 | 2026-07-19 | Report K8s 1.36 API posture (/version, discovery); 1.33-1.36 served group-versions unchanged, substantive deltas are node-side (#37) |
| v0.7.22 | 2026-07-19 | CRD establishing: created CRDs get status (acceptedNames + NamesAccepted/Established + storedVersions) so clients stop hanging (#36) |
| v0.7.21 | 2026-07-19 | CRITICAL: protobuf decode uses endpoint GVK when envelope TypeMeta is blank — typed client-go clients (cilium CRD create) now work (#34) |
| v0.7.20 | 2026-07-19 | Cert-expiry monitoring: apiserver_certificate_expiration_seconds metric + near-expiry warnings (#20 Phase 1a) |
| v0.7.19 | 2026-07-19 | CRITICAL: protobuf codec for apiextensions CRDs (schema union types) + policy/autoscaling/scheduling/admission/certificates — unblocks cilium CRD creation (#34) |
| v0.7.18 | 2026-07-19 | Node draining: policy/v1 PodDisruptionBudget + PDB-gated pod Eviction subresource (429) + PDB status controller (#7) |
| v0.7.17 | 2026-07-19 | Security hardening: refuse plain HTTP without --insecure; anonymous no longer cluster-admin unless --dev-anonymous-admin (#16) |
| v0.7.16 | 2026-07-19 | CRITICAL: resourceVersion sourced from store mod_revision, not stale baked-in JSON — fixes optimistic concurrency & all leader election (#33) |
| v0.7.15 | 2026-07-19 | Emit core/v1 Events (SuccessfulCreate/Delete, ScalingReplicaSet) + event TTL GC (#15); richer apiserver metrics — latency histogram, verb/resource/code, inflight (#13) |
| v0.7.14 | 2026-07-19 | client-go protobuf wire codec (application/vnd.kubernetes.protobuf), both directions — unblocks cilium-operator & all client-go controllers (#32) |
| v0.7.13 | 2026-07-19 | OpenAPI v3 paths declare GVK + fieldValidation so `kubectl apply` stops falling back to protobuf v2 (#31) |
| v0.7.12 | 2026-07-19 | Watch tombstones carry TypeMeta (fixes client-go informers); serve /openapi/v2+v3 so `kubectl apply` validates |
| v0.7.11 | 2026-07-18 | Serve storage.k8s.io/v1 — StorageClass, CSIDriver, CSINode, VolumeAttachment, CSIStorageCapacity (#24) |
| v0.7.10 | 2026-07-18 | CR PATCH (merge/JSON-patch/apply) + CR `/status` subresource; PATCH on built-in resources (#23) |
| v0.7.9 | 2026-07-18 | SA token auth: stable RS256 signing keypair across replicas (#11, #29) + default `kubernetes` Service/Endpoints (#30) |
| v0.7.8 | 2026-07-18 | Namespace deletion cascade — graceful Terminating + `/finalize` + controller purges contained resources (#28) |
| v0.7.7 | 2026-07-17 | Fix ReplicaSet/DaemonSet unbounded pod storm — GC terminal pods + exponential recreate backoff (#27) |
| v0.7.6 | 2026-07-17 | Fix LIST pagination hang — percent-decode `continue` token + label/field selectors in query parser |
| v0.7.5 | 2026-07-16 | Serve EndpointSlices (discovery.k8s.io/v1) in apiserver + controller-manager (#22) |
| v0.7.4 | 2026-07-16 | Fix CRD endpoints 500 — drop redundant apiextensions route (#21) |
| v0.7.3 | 2026-07-16 | Watch-cache stall re-seed (#18); pin fastetcd v0.8.2 (#8) |
| v0.3.0 | 2026-03-19 | Phase 2/3 — status subresources, admission, CSI, netpol, eBPF, HPA, Gateway, aggregation, cloud |
| v0.2.0 | 2026-03-18 | Label/field selectors, auth/RBAC, workload controllers, CRD support |
| v0.1.0 | 2026-03-17 | Initial scaffold — all 10 crates fully implemented |
