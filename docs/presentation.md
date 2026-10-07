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
Checked 2026-09-29 against v0.18.0 plus unreleased turbomode and README.md.
Branch code is not a shipped golden or a conformance claim. The file named on each slide is
where to check it.
-->

# RustKube

**A Kubernetes control plane in Rust**

`kube-apiserver` · `kube-controller-manager` · `kube-scheduler`

Rust · one cargo workspace · three binaries, static musl
Version 0.18.0 + unreleased turbomode (`Cargo.toml` `[workspace.package]`)

---

## What it is and the problem it solves

stormcos needs a Kubernetes control plane that is **small, static and ours**:
one binary per component, no Go toolchain, no distro images, shipped as a
stormd golden like everything else on the node.

RustKube speaks the **Kubernetes API on the wire** — JSON *and* client-go's
protobuf — for existing `kubectl`, `oc`, `helm` and client-go clients.
Compatibility depends on the operation; it is not yet conformant or a
drop-in upstream replacement.

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
             kube-controller-manager ──────┤  watch-driven work queues,
             kube-scheduler ───────────────┤  write back through the API
                                           │
                                           └──HTTPS :10250──▶ kubelet (rustkube-node)
                                                logs, exec, attach, port-forward
```

- Keys `/registry/{resource}/[{ns}/]{name}`, CRs `/registry/{group}/{plural}/…`
- `resourceVersion` = fastetcd's `mod_revision`; every write is a CAS
- Shared informers and indexed object workers drive controllers;
  scheduler placement is serialized. Snapshot safety needs fastetcd ≥ v1.6.1 (fastetcd#50, fixed).

---

## Today: the apiserver (`pkg/apiserver`)

- **Groups:** core, apps, batch, autoscaling, policy, networking,
  discovery, events, coordination, rbac, authorization, certificates,
  storage, apiextensions (CRDs), gateway, `route.openshift.io`,
  `project.openshift.io`, kubevirt subresources. Admission webhooks are
  called on every write (#82); APIServices with a service are proxied (#83).
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
- **AuthZ:** RBAC, plus Kubernetes access reviews (`oc auth can-i`); OpenShift
  `oc policy who-can` needs unserved reviews (#106) (`rbac_engine.rs`, `handlers/authorization.rs`).
- **Admission (built-in):** namespace lifecycle, service IP allocation,
  default ServiceAccount, tolerations, priority, PodSecurity subset,
  `ReadWriteOncePod` (`builtin_admission.rs`).
- **Projects (#97):** `oc new-project` gives the requester `admin` in a new
  namespace; `oc projects` lists only one's own; `admin`/`edit`/`view` are
  bootstrapped (`handlers/project.rs`).

---

## Today: controllers and scheduler

**Controller families** (`pkg/controller-manager/src/runner.rs`): Deployment,
ReplicaSet, StatefulSet, DaemonSet, Job, CronJob, Service (Endpoints +
EndpointSlices), Namespace cascade, node lifecycle, PDB, garbage collector,
PersistentVolume binding, attach/detach, the in-kubelet `stormblock`
provisioner, root CA publisher, CSR, PodMigration, VirtualMachine, VMI launcher Pods, VMI migration, HPA\*,
Gateway\*.

**Scheduler** (`pkg/scheduler`): filters — readiness, taints, selectors,
node and pod (anti-)affinity, topology spread, resource fit, volume binding
incl. `CSIStorageCapacity` and `ReadWriteOncePod`; scores summed; VMIs placed.

\* HPA on cpu/memory from cadvisor via `metrics.k8s.io`; pod metrics on stormcos wait for cadvisor#3 (#89); Gateway writes status only, for its own classes, `Programmed=False` (#70, #91).

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
  stormpump from `boot.d/30-kube` on profiles enabling the control plane.
- stormd supervises: restart on failure, ready probes (fastetcd's port, then
  the apiserver's `/healthz`), rotated logs on a log volume.
- **At boot** the apiserver waits for fastetcd, creates the default
  namespaces and bootstrap RBAC, registers in `default/kubernetes`, applies
  the manifests. Serving certs reload without a restart.
- **Update** = a new golden, rolled by a stormcos release.

---

## How it is tested

- **Unit tests:** 409 passed, 4 ignored at 0c455b8 through sc-build (five
  core libraries); this is not a runtime or conformance pass.
- **End to end on dev** (`test/e2e/`): a real apiserver and
  controller-manager on a fresh fastetcd —
  - `projects.sh` — `oc` 4.22 as two users and an admin (31 checks)
  - `status-rv.sh` — optimistic concurrency on every status handler (17)
  - `watch-deleted.sh` — DELETED events for CRs and built-ins (7)
- **Conformance has run:** synthetic nodes, no kubelets; 75/428 passed
  at efbea2d before the kubectl rerun. Not certification (docs/conformance.md).
- **Test container exists:** `/test short|medium|long`; real Job acceptance
  remains #96. Scale and multi-master acceptance remain #66/#147/#149.

---

## Planned — not in the code yet

Built as modules but **not wired**:
- scheduler preemption (`preemption.rs`, #84)

Missing:
- `/scale` subresource — `kubectl scale` fails (#86) · `schedulingGates` (#87)
- pod metrics on stormcos: cadvisor attributing stormpump cgroups (cadvisor#3)
- generic ephemeral volumes (#94); expansion and snapshot API integration
  now have upstream-sidecar tests (#63/#64), not full node acceptance
- Node authorizer
- validation of turbomode informers at scale and under failover (#146/#149)

---

## Status and open issues that matter

- **#147 / #149 — turbomode live acceptance.** fastetcd#50 (inconsistent
  LIST snapshots) is fixed in v1.6.1 and the rig verifies it; latency at
  scale and three-master failover are still unproven on a live cluster.
- **stormcos#76 — partial auth wiring now exists.** Its build script supplies
  controller client certs; `sno`/`bastion` still grant anonymous-admin.
  Build configuration is not proof of deployment.
- **rustkube-node#56** — the kubelet serves no exec/attach/port-forward, so
  `oc rsh`/`cp`/`port-forward` stop at the node.
- **#86** — `kubectl scale`: what a typical operator install trips over
  first, now that webhooks are called (#82).

Every open issue: `gh issue list -R glennswest/rustkube`.
