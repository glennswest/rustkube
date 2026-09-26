---
marp: true
theme: default
paginate: true
title: rustkube
description: Purpose and functionality of RustKube, the Kubernetes control plane in Rust
style: |
  section { font-size: 24px; }
  pre { font-size: 0.72em; }
  table { font-size: 0.85em; }
---

<!--
Render: npx @marp-team/marp-cli docs/presentation.md          (HTML)
        npx @marp-team/marp-cli docs/presentation.md --pdf    (PDF)
Every claim here is drawn from the code as of v0.15.2 and from README.md,
which was rewritten from the code (#80). The file named on each slide is
where to check it.
-->

# RustKube

**A Kubernetes control plane in Rust**

`kube-apiserver` · `kube-controller-manager` · `kube-scheduler`

Rust · one cargo workspace · three binaries, static musl
Version 0.15.2 (`Cargo.toml` `[workspace.package]`)

---

## What it is and the problem it solves

stormcos needs a Kubernetes control plane that is **small, static and ours**:
one binary per component, no Go toolchain, no distro images, shipped as a
stormd golden like everything else on the node.

RustKube speaks the **Kubernetes API on the wire** — JSON *and* client-go's
protobuf — so the tools and controllers written for Kubernetes work against
it unchanged: `kubectl`, `oc`, `helm`, Cilium, the CSI sidecars.

It is the control plane only. The datastore (fastetcd), the kubelet
(rustkube-node) and cluster DNS (stormcoredns) are separate components.

---

## Where it sits in stormcos

```
          depends on                                   depended on by
  fastetcd ── etcd v3 gRPC :2379 ──┐            ┌── rustkube-node  (kubelet: registers,
  stormcert ── certs, SA keypair ──┤            │                   serves pod logs)
  stormd ──── PID 1 of each golden ┤            ├── stormblock-csi, network-operator,
  stormlb ─── kube-api VIP in front┤            │   stormipmi, stormconsole, sc
                                   ▼            │   (all clients of its API)
                            ┌──────────────┐    │
                            │   rustkube   │────┘
                            └──────────────┘◄──── stormcos (ships it: 3 stormd goldens)
```

Relationships as in `stormcentral check`: rustkube → fastetcd, stormcert,
stormlb, stormd. stormlb is the VIP that fronts the apiservers and reads
Services/HTTPRoutes from them; rustkube's code never calls it.

---

## How it works

```
kubectl / oc / client-go ──HTTPS :6443──▶ kube-apiserver ──gRPC──▶ fastetcd :2379
                                           │  auth → RBAC → admission → storage
                                           │  one watch cache per prefix
                                           │
             kube-controller-manager ──────┤  poll + list every 2–30 s,
             kube-scheduler ───────────────┤  write back through the API
                                           │
                                           └──HTTPS :10250──▶ kubelet (rustkube-node)
                                                logs, exec, attach, port-forward
```

- Objects are `serde_json::Value` end to end; keys are
  `/registry/{resource}/[{ns}/]{name}`, custom resources
  `/registry/{group}/{plural}/…` (`pkg/apiserver/src/storage.rs`).
- `resourceVersion` is fastetcd's `mod_revision`; every write is a CAS.
- Controllers and the scheduler are API clients like any other — no informers.

---

## Today: the apiserver (`pkg/apiserver`)

- **Groups:** core, apps, batch, autoscaling, policy, networking,
  discovery, events, coordination, rbac, authorization, certificates,
  storage, apiextensions (CRDs), gateway, `route.openshift.io`,
  `project.openshift.io`, kubevirt subresources. Webhook configurations and
  APIServices are stored but not acted on (#82, #83).
- **Wire:** JSON + protobuf, Table output, `PartialObjectMetadata`, watch with
  bookmarks and `sendInitialEvents`, pagination, label/field selectors.
- **Writes:** create/update/delete with `DeleteOptions`, JSON / merge /
  strategic-merge patch, server-side apply with `managedFields`, `/status`
  conditional on `resourceVersion` (#78).
- **Watch:** DELETED carries the last state; selectors apply (#100).
- **Proxied to the kubelet:** `pods/log`, `exec`, `attach`, `portforward`.

---

## Today: security and multi-tenancy

- **AuthN:** x509 client certs (`--client-ca-file`), RS256 ServiceAccount
  JWTs (`--service-account-*-file`), TokenRequest, TokenReview
  (`pkg/apiserver/src/auth.rs`).
- **AuthZ:** RBAC, plus the four access reviews `oc auth can-i` and
  `oc policy who-can` use (`rbac_engine.rs`, `handlers/authorization.rs`).
- **Admission (built-in):** namespace lifecycle, service IP allocation,
  default ServiceAccount, tolerations, priority, PodSecurity subset,
  `ReadWriteOncePod` (`builtin_admission.rs`).
- **Projects (#97):** `oc new-project` gives the requester `admin` in a new
  namespace; `oc projects` lists only one's own; `admin`/`edit`/`view` are
  bootstrapped (`handlers/project.rs`).

---

## Today: controllers and scheduler

**19 controllers** (`pkg/controller-manager/src/runner.rs`): Deployment,
ReplicaSet, StatefulSet, DaemonSet, Job, CronJob, Service (Endpoints +
EndpointSlices), Namespace cascade, node lifecycle, PDB, garbage collector,
PersistentVolume binding, attach/detach, the in-kubelet `stormblock`
provisioner, CSR, PodMigration, VirtualMachine, HPA\*, Gateway\*.

**Scheduler** (`pkg/scheduler`): filters — readiness, taints, selectors,
node and pod (anti-)affinity, topology spread, resource fit, volume binding
incl. `CSIStorageCapacity` and `ReadWriteOncePod`; scores summed; VMIs placed.

\* placeholders: HPA reads no metrics (#89); Gateway writes status only (#91).

---

## Interfaces

| | |
|---|---|
| API | HTTPS `--secure-port` 6443; `/healthz`, `/livez`, `/readyz`, `/version`, `/metrics` |
| controller-manager | plain HTTP :10257 — `/metrics`, `/healthz` |
| scheduler | plain HTTP :10259 — `/metrics`, `/healthz` |
| datastore | `--etcd-servers` (required), optional mutual TLS |
| config | flags + a few env vars (`ETCD_SERVERS`, `APISERVER_URL`, `APISERVER_TOKEN`, …), `RUST_LOG`; no config file, no `--kubeconfig` |
| manifests | `--manifest-dir`: applied once at boot, in filename order |

Every flag, its env var and default: README → *Configuration*.
Metric names follow upstream's: `docs/metrics.md`.

---

## How it ships and is operated

- **Built from source** on dev.g8.lo (`sc-build`), never as root.
  `stormcentral component build rustkube` makes the component golden
  `golden-rustkube-<digest>` (the three binaries) and files the stormcos
  release request.
- stormcos wraps each binary in a **stormd golden** —
  `rustkube-apiserver`, `-controller-manager`, `-scheduler` — started by
  stormpump from `boot.d/30-kube` on the `sno` and `storage` profiles.
- stormd supervises: restart on failure, ready probes (fastetcd's port, then
  the apiserver's `/healthz`), rotated logs on a log volume.
- **At boot** the apiserver waits for fastetcd, creates the default
  namespaces and bootstrap RBAC, registers in `default/kubernetes`, applies
  the manifests. Serving certs reload without a restart.
- **Update** = a new golden, rolled by a stormcos release.

---

## How it is tested

- **Unit tests:** ~330 across the workspace, `cargo test` via `sc-build`;
  handlers run against an in-memory store with etcd's CAS semantics
  (`pkg/apiserver/src/test_store.rs`).
- **End to end on dev** (`test/e2e/`): a real apiserver and
  controller-manager on a fresh fastetcd —
  - `projects.sh` — `oc` 4.22 as two users and an admin (31 checks)
  - `status-rv.sh` — optimistic concurrency on every status handler (17)
  - `watch-deleted.sh` — DELETED events for CRs and built-ins (7)
- **Not yet:** the Kubernetes conformance suite (#67), a run past 250
  synthetic nodes (#66), test containers per the stormcos standard (#96).

---

## Planned — not in the code yet

Built as modules but **not wired**:
- admission webhooks (`admission.rs`, #82) · API aggregation
  (`aggregation.rs`, #83) · scheduler preemption (`preemption.rs`, #84)

Missing:
- `/scale` subresource — `kubectl scale` fails (#86) · `schedulingGates` (#87)
- a real HPA with a metrics API (#89, needs #83)
- volume expansion (#63), snapshots (#64), generic ephemeral volumes (#94)
- RBAC escalation prevention (#98) · Node authorizer
- informer-based controllers, if scale says so (#66)

---

## Status and open issues that matter

- **#99 — the GC deletes a live Deployment's ReplicaSet** as "owner gone",
  repeatedly. Deleting live objects: first in line.
- **#98 — no RBAC escalation check.** Contained for projects by keeping
  Namespace writes cluster-scoped; fixing it lifts that limit.
- **stormcos#76 — the control plane runs anonymous.** Controllers get only a
  CA; fine on `sno` (dev anonymous-admin), refused on `storage`.
- **rustkube-node#56** — the kubelet serves no exec/attach/port-forward, so
  `oc rsh`/`cp`/`port-forward` stop at the node.
- **#82, #86, #85** — webhooks, `kubectl scale`, discovery gaps: what a
  typical operator install trips over first.

Every open issue: `gh issue list -R glennswest/rustkube`.
