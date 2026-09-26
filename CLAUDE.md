# CLAUDE.md — RustKube Project Instructions

## Project Overview

RustKube is a Kubernetes **control plane** in Rust — `kube-apiserver`,
`kube-controller-manager`, `kube-scheduler` — wire-compatible with kubectl,
oc, helm and client-go. Target scale: 100–1000+ nodes (largest run: a
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

dev.g8.lo's `/tmp` is not writable by the build user: prefix commands with
`mkdir -p $HOME/tmp && export TMPDIR=$HOME/tmp &&` (etcd-client's build
script needs a temp dir). `protoc` is required (`pkg/apimachinery/build.rs`).

## Workspace Structure

```
cmd/
  kube-apiserver/            binary → apiserver
  kube-controller-manager/   binary → controller-manager
  kube-scheduler/            binary → scheduler
pkg/
  apimachinery/       errors, KvStore trait, protobuf codec, metrics, quantities, selectors, cron, startup waits
  storage/            etcd v3 client (etcd-client) — keys are opaque here
  apiserver/          REST API (axum), auth, RBAC, built-in admission, watch cache, CRDs
  scheduler/          fixed filter/score functions, volume binding, VMI placement
  controller-manager/ the built-in controllers (poll + list, no informers)
  cloud/              EMPTY — a doc comment, no code, nothing depends on it
```

Objects are `serde_json::Value` throughout; no k8s-openapi types are used.

**Built but not wired** — modules whose docs used to read as features:
`apiserver::admission` (webhooks, #82), `apiserver::aggregation` (#83),
`scheduler::preemption` (#84), `scheduler::plugins` (unused traits),
`apimachinery::{rbac, meta}` (unused types/helpers).

## Version Locations

```
Cargo.toml → workspace.package.version
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

## Current Version: `v0.15.3`

## Work Plan

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

### Found by the docs pass (#80), open
- [ ] Admission webhooks are never called (#82)
- [ ] Aggregation proxies nothing (#83)
- [ ] Scheduler never preempts (#84); ignores `schedulingGates` (#87)
- [ ] PriorityClass / TokenReview missing from `/apis` (#85)
- [ ] No `/scale` subresource; `kubectl scale` fails (#86)
- [ ] `--data-dir`, `--cluster-domain` accepted and unused (#88)
- [ ] HPA placeholder: no metrics, never scales down (#89)
- [ ] Metrics: reconcile metrics never emitted, no histogram buckets,
      unauthenticated apiserver `/metrics` (#90)
- [ ] Gateway controller: hardcoded address, overwrites foreign classes (#91)
- [ ] Serving-cert reload applies a mismatched key/cert pair (#93)
- [x] `/status` PUT is conditional on the body's `resourceVersion` (#78),
      all four handlers; `test/e2e/status-rv.sh`

### Open, found since the docs pass
- [x] GC deleted a live Deployment's ReplicaSet (#99): protobuf creates were
      stored with `uid: ""`; fixed in v0.15.3
- [ ] NotFound names the store key (#109); unserved resources answer an empty
      list (#110); LIST items carry no `resourceVersion` (#111)
- [ ] RBAC escalation prevention (#98); until then Namespace writes stay
      cluster-scoped (#97)
- [ ] Secrets: `stringData` not folded into `data` (#101)
- [ ] PVC `status.phase` not defaulted to `Pending` on create (#102)
- [ ] No generic ephemeral-volume controller (#94)
- [ ] Test containers per the stormcos test standard (#96); the e2e scripts
      in `test/e2e/` (lib.sh + projects, status-rv, watch-deleted) are the
      start of it

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

### Storage (v0.8.0)
- [x] PV/PVC binding, protection finalizers, phases, reclaim, events (#56)
- [x] Attach/detach — `VolumeAttachment` for drivers that require it
- [x] Volume-aware scheduling — PV `nodeAffinity`, `selected-node`,
      `CSIStorageCapacity`
- [ ] Volume expansion — `status.allocatedResources` + resize conditions (#63)
- [ ] Snapshots — the external-snapshotter CRDs and controller (#64)
- [x] `ReadWriteOncePod` enforcement — scheduler + admission (#65); the
      kubelet's mount refusal is rustkube-node#42
- [x] In-kubelet `stormblock` class: `stormblock.rs` writes the PV once the
      scheduler picks a node (#71, v0.13.0–v0.14.1)
- [ ] `stormblock.rs` matches the class by name, which stormblock-csi's
      class also uses (#92)
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

### Phase 4: Scale & Conformance
- [ ] 1000+ node testing (#66) — the controllers list everything every tick,
      which is what will break first
- [ ] K8s conformance test suite (#67)
- [ ] ARM64 cross-compile verification + MikroTik minimal build (#68) — CI
      builds x86_64 musl only; `build-release.sh` can target aarch64 via
      `cross`, and no such build has been recorded

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
      `adm new-project` (#106), aggregated discovery for `inspect` (#107),
      `nodes/proxy` for `node-logs` (#108); `top` needs #83
- [ ] `oc scale` — no `/scale` (#86).
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
