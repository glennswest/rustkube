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
and others are left from the 10-crate layout. k8s-openapi (0.24 `v1_32`,
never imported) was removed for #285.

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

## Work plan and history

Moved to [docs/work-plan.md](docs/work-plan.md) (CLAUDE.md is loaded into every session; its size made the session's context thrash). Nothing was dropped. Not loaded by default: `grep -n '^##' docs/work-plan.md`, then read only the section you need. Write new intentions and progress there, not here.

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
