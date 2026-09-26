# RustKube

**A Kubernetes control plane in Rust**: `kube-apiserver`,
`kube-controller-manager` and `kube-scheduler`, speaking the Kubernetes API on
the wire, so `kubectl`, `oc`, `helm` and client-go controllers (Cilium, the CSI
sidecars) work against it unchanged.

It is the control plane only. The datastore, the node agent and DNS are
separate components:

| | component | what it is to rustkube |
|---|---|---|
| datastore | [fastetcd](https://github.com/glennswest/fastetcd) | an etcd v3 wire-compatible store; the apiserver is its only client, over gRPC (`--etcd-servers`) |
| node agent | [rustkube-node](https://github.com/glennswest/rustkube-node) | the kubelet; the apiserver proxies `logs`, `exec`, `attach` and `port-forward` to it on `:10250` |
| certificates | [stormcert](https://github.com/glennswest/stormcert) | writes the serving cert, the CA and the ServiceAccount keypair the apiserver reads |
| cluster DNS | stormcoredns (CoreDNS as a fallback), deployed from a manifest | rustkube knows nothing about DNS |
| where it runs | [stormcos](https://github.com/glennswest/stormcos) | ships each binary as a stormd golden — see [How it ships](#how-it-ships) |

```
kubectl / oc / client-go ──HTTPS :6443──▶ kube-apiserver ──gRPC──▶ fastetcd :2379
                                              ▲    │
                        kube-controller-manager    └─HTTPS :10250─▶ kubelet (rustkube-node)
                        kube-scheduler              logs, exec, attach, port-forward
```

The target is 100–1000+ nodes. The largest run so far is a synthetic
250-node, 3000-pod cluster (#66); nothing has run at that size with real nodes.

## Layout

Upstream-shaped: thin `cmd/<component>` binaries over `pkg/<lib>` libraries.

```
cmd/kube-apiserver           → pkg/apiserver           REST API (axum), auth, RBAC, admission, watch cache
cmd/kube-controller-manager  → pkg/controller-manager  the built-in controllers
cmd/kube-scheduler           → pkg/scheduler           filter / score / bind
                               pkg/apimachinery        errors, the KvStore trait, protobuf codec, metrics, quantities, selectors, cron
                               pkg/storage             the KvStore implementation over etcd v3 (etcd-client)
                               pkg/cloud               empty: a doc comment and no code; nothing depends on it
```

## What the apiserver does

**Storage.** Objects are JSON under `/registry/{resource}/…` in fastetcd;
custom resources under `/registry/{group}/{plural}/…` (#76). `resourceVersion`
is the store's `mod_revision`. Every write is a compare-and-swap; a PATCH
without a `resourceVersion` retries the CAS the way upstream's
`GuaranteedUpdate` does (#77). A watch cache sits in front of watches.

**API groups served** (and advertised in `/api`, `/apis`):
`v1`, `apps/v1`, `batch/v1`, `autoscaling/v2`, `policy/v1`,
`networking.k8s.io/v1`, `discovery.k8s.io/v1`, `events.k8s.io/v1` (translated
to and from stored core/v1 Events), `coordination.k8s.io/v1`,
`rbac.authorization.k8s.io/v1`, `authorization.k8s.io/v1`,
`certificates.k8s.io/v1`, `storage.k8s.io/v1`, `admissionregistration.k8s.io/v1`,
`apiextensions.k8s.io/v1` (CRDs, served dynamically; no schema validation or
conversion), `apiregistration.k8s.io/v1` (APIService objects are stored, but
nothing is proxied to them, #83),
`gateway.networking.k8s.io/v1`, `route.openshift.io/v1` (stored; nothing
routes for it, #70), `project.openshift.io/v1` (Projects, below),
`rustkube.io/v1alpha1` (PodMigration) and
`subresources.kubevirt.io/v1` (VM console/VNC, proxied to the kubelet).

`scheduling.k8s.io/v1` (PriorityClass) and `authentication.k8s.io/v1`
(TokenReview) are **served but not advertised** in `/apis` (#85), so a client
that discovers first — `kubectl get priorityclasses` — does not find them.

**Wire.** JSON and client-go's protobuf (`application/vnd.kubernetes.protobuf`)
in both directions; Table output for `kubectl get`; `PartialObjectMetadata`;
watch with bookmarks and `sendInitialEvents` (a DELETED event carries the
object's last state from the watch cache, and selectors apply to it; a watch
opened below the cache's window gets a name-and-namespace tombstone, #100);
list pagination with `continue`
tokens; label and field selectors; `/openapi/v2` and `/openapi/v3`.

**Writes.** Create, update, delete (with `DeleteOptions`: preconditions,
`dryRun`, grace period, `propagationPolicy`), JSON Patch, merge patch,
strategic merge patch (with `patchMergeKey` for the lists that need it) and
server-side apply with `managedFields` ownership and conflicts. The `/status`
subresource; pod `eviction` gated by PodDisruptionBudgets; namespace
`/finalize`; CSR `/approval`. A PUT to `/status` (and `/approval`) is
conditional on the body's `resourceVersion`: stale is a 409 and nothing is
written; none is an unconditional update (#78). There is **no `/scale`** subresource, though
discovery advertises `deployments/scale`, so `kubectl scale` fails (#86).

**Proxied to the kubelet** (`https://<node>:10250`, authenticated with a token
the apiserver mints for itself as `system:kube-apiserver`): `pods/log`
(including `follow`), and `pods/exec`, `pods/attach`, `pods/portforward` as a
transparent connection upgrade, so SPDY and WebSocket both pass through. The
kubelet end of exec/attach/port-forward does not exist yet in rustkube-node
(rustkube-node#56), so those three answer with the kubelet's 404 today.

**Authentication**, first match wins:
1. an x509 client certificate verified against `--client-ca-file` — CN is
   the user, each O a group;
2. a bearer JWT signed with the ServiceAccount key (RS256 with
   `--service-account-*-file`, otherwise an ephemeral HS256 key). A
   ServiceAccount's groups come from its name. What an offline-minted token
   must carry is in [docs/certificates.md](docs/certificates.md);
3. otherwise `system:anonymous`, if `--anonymous-auth` is true; else 401.

TokenRequest (`serviceaccounts/{name}/token`) mints an unbound token with a
fixed 24-hour lifetime; the request body, `expirationSeconds` included, is
ignored. TokenReview is served.

**Authorization** is RBAC against the stored Roles and Bindings, plus
SelfSubjectAccessReview, SelfSubjectRulesReview, SubjectAccessReview and
LocalSubjectAccessReview.

**Projects** (`project.openshift.io/v1`, #97) are Namespaces with owners.
Nothing is stored as a Project: each is the Namespace of the same name,
translated on the way out, so deleting a project deletes its namespace and
the namespace cascade takes everything in it.

- `oc new-project` (a ProjectRequest) is open to every authenticated user
  through the `self-provisioners` ClusterRoleBinding. It creates the Namespace
  annotated `openshift.io/requester` (from the authenticated identity, never
  the request), `openshift.io/display-name` and `openshift.io/description`,
  and a RoleBinding `admin` giving the requester the `admin` ClusterRole in
  it. `default`, `openshift` and names starting `kube-` or `openshift-` may
  not be requested. To turn self-service off, empty the subjects of
  `self-provisioners` (a boot does not restore them).
- `oc projects` / `oc get projects` list only the namespaces the caller holds
  a RoleBinding in — any role — unless the caller may `list namespaces`
  cluster-wide, who sees all. Watching projects is limited to the latter.
- `get`, `update` and `delete` of `projects/{name}` are authorized in the
  project's own namespace, so a project's `admin` can delete it and nobody
  else's. So is `get namespaces/{name}`, as upstream does. A project update
  changes only its display name and description; the Namespace's labels
  (`pod-security.kubernetes.io/enforce` among them) stay cluster-scoped to
  write, because nothing here stops a project admin binding cluster-admin
  inside their own project (#98).
- Sharing is RBAC: `oc adm policy add-role-to-user edit bob -n demo` binds
  one of `admin` (edit + roles/rolebindings + delete the project), `edit`
  (write workloads, read secrets, exec/attach/port-forward, VM start/stop)
  or `view` (read all but secrets).

**Admission**, on create: NamespaceLifecycle, namespace defaults (`Active`,
the `kubernetes` finalizer), Service port defaults and ClusterIP allocation
from `--service-cidr`, the pod's default ServiceAccount, the not-ready /
unreachable tolerations, priority from its PriorityClass, a subset of
PodSecurity keyed on the namespace's `pod-security.kubernetes.io/enforce`
label, CronJob schedule validation, and PVC access-mode validation
(`ReadWriteOncePod` may not be combined with another mode). That is the
whole chain: **admission webhooks are not called**. Webhook configurations
are stored and served, and no request reaches a webhook (#82).

**At boot**, idempotently: waits for the datastore; creates the `default`,
`kube-system`, `kube-public` and `kube-node-lease` namespaces and backfills
older namespaces' phase and finalizer; migrates pre-#76 custom-resource keys;
creates the bootstrap RBAC (below); registers itself in the `default/kubernetes`
Service and Endpoints; then applies `--manifest-dir`.

Bootstrap RBAC: `cluster-admin`; `system:masters`, `system:nodes`,
`system:kube-controller-manager` and `system:kube-scheduler` bound to it;
`system:node-bootstrapper` (CSR create) for `system:bootstrappers`;
`system:basic-user` (self-reviews) for `system:authenticated`;
`system:discovery` for `system:anonymous`; the project roles `admin`,
`edit`, `view`, `basic-user` (list one's projects) and `self-provisioner`,
the latter two bound to `system:authenticated`; and the ServiceAccount
`kube-system/node-admin` bound to `cluster-admin` for a node's ssh login
(#79). The project roles are reconciled at every boot — a stored copy's
rules are brought up to date unless it is annotated
`rbac.authorization.kubernetes.io/autoupdate: "false"`; everything else is
created once. With `--dev-anonymous-admin` it also binds `system:anonymous` to
`cluster-admin`; without it, a binding left from an earlier boot is removed.

## What the controller manager runs

One process, leader-elected on the Lease `kube-system/kube-controller-manager`.
Every controller reconciles by **listing its objects on a fixed interval**
(2–30 s) and following `continue` tokens to the end; none uses a watch or an
informer cache (#66).

Deployment (rolling updates), ReplicaSet, StatefulSet, DaemonSet (every
eligible node, Ready or not; places pods itself), Job, CronJob, Service (Endpoints and EndpointSlices), Namespace (default
ServiceAccount, deletion cascade), node lifecycle (Lease heartbeats →
NotReady → eviction), PodDisruptionBudget status, garbage collection
(background, foreground and orphan, driven by discovery), PersistentVolume
(binding, phases, protection finalizers, reclaim), attach/detach
(VolumeAttachment), the stormblock provisioner for the in-kubelet `stormblock`
class, CSR approval and signing (auto-approves only the
`kubernetes.io/kube-apiserver-client-kubelet` signer; signs only with
`--cluster-signing-*-file`), PodMigration, and VirtualMachine
(`start`/`stop`/`restart`). Events are emitted for creates, deletes and
scaling, and expired ones are deleted.

Two are placeholders: **HPA** reads no metrics — its "utilization" is the
fraction of Ready pods, and it never scales down (#89) — and **Gateway API**
writes status only, with a hardcoded address (#91).

It has no ResourceQuota, ServiceAccount-token, TTL-after-finished or
node-IPAM controller.

## What the scheduler does

Leader-elected on `kube-system/kube-scheduler`. It polls for pods with no
`spec.nodeName` every second and binds each to the best feasible node.

- **Filters:** node Ready, not unschedulable, taints/tolerations,
  `nodeSelector`, required node affinity (which is how `kubernetes.io/arch`
  is enforced), `nodeName`, inter-pod affinity and anti-affinity, topology
  spread (`DoNotSchedule`), resource fit (pod-level requests honoured, #73),
  and volume binding: PV node affinity, `CSIStorageCapacity`,
  `ReadWriteOncePod`, and `selected-node` for `WaitForFirstConsumer` claims.
- **Scores**, summed: least requested, image locality, preferred node
  affinity, preferred pod affinity, and topology spread (`ScheduleAnyway`).
- VirtualMachineInstances are scheduled too (#72).

It **does not preempt**: `preemption.rs` computes victims but nothing calls it
(#84). It **ignores `schedulingGates`** and binds gated pods (#87). There is
no scheduling queue with backoff and no `nominatedNodeName`. `plugins.rs`
defines plugin traits the loop does not use.

## Configuration

Every binary logs through `RUST_LOG` (default `info`).

### kube-apiserver

| flag | env | default | |
|---|---|---|---|
| `--etcd-servers` | `ETCD_SERVERS` | **required** | fastetcd endpoints, comma-separated |
| `--etcd-cacert`, `--etcd-cert`, `--etcd-key` | `ETCD_CACERT`, `ETCD_CERT`, `ETCD_KEY` | — | TLS / mutual TLS to fastetcd |
| `--bind-addr` | | `0.0.0.0` | |
| `--secure-port` | | `6443` | |
| `--tls-cert-file`, `--tls-private-key-file` | | — | serving cert; **reloaded when the files change**, no restart |
| `--tls` | | off | serve a self-signed cert generated at start, held in memory only; DNS SANs `kubernetes…` and `localhost`, no IP SANs |
| `--insecure` | | `false` | allow plain HTTP when no TLS is configured; without it the server refuses to start |
| `--client-ca-file` | | — | enables x509 client-certificate authentication |
| `--anonymous-auth` | | `true` | `false` answers 401 to unauthenticated requests |
| `--dev-anonymous-admin` | | `false` | **dev only**: anonymous is `cluster-admin` (needs `--anonymous-auth true`) |
| `--service-account-signing-key-file` | | — | RSA private key (PEM) that signs tokens |
| `--service-account-key-file` | | — | its public key (SPKI PEM). Both or neither; neither means an ephemeral key that dies with the process |
| `--advertise-address` | | `--bind-addr` if concrete | the address put in `default/kubernetes` Endpoints |
| `--service-cidr` | | `10.96.0.0/12` | ClusterIP range; `.1` is the `kubernetes` Service |
| `--manifest-dir` | `MANIFEST_DIR` | — | YAML/JSON applied once at start, in filename order; created if absent, overwritten if annotated `addonmanager.kubernetes.io/mode: Reconcile` |
| `--data-dir` | | `/var/lib/kubernetes` | accepted and **unused** (#88) |
| `--cluster-domain` | | `cluster.local` | accepted and **unused** (#88) |

### kube-controller-manager and kube-scheduler

| flag | env | default | |
|---|---|---|---|
| `--apiserver` | `APISERVER_URL` | `http://127.0.0.1:6443` | an `https://` URL turns on TLS |
| `--certificate-authority` | | — | CA bundle for the apiserver |
| `--client-certificate`, `--client-key` | | — | mutual TLS identity |
| `--token` | `APISERVER_TOKEN` | — | bearer token |
| `--token-file` | | — | bearer token from a file |
| `--insecure-skip-tls-verify` | | off | |
| `--leader-elect` | | `true` | |
| `--startup-timeout` | `STARTUP_TIMEOUT` | `120` | seconds to wait for credential files and for the apiserver |
| `--cluster-signing-cert-file`, `--cluster-signing-key-file` | | — | controller manager only: the CA the CSR controller signs with; without them CSRs are approved but not signed |

Neither takes `--kubeconfig`. A credential file that does not exist yet is
waited for, not treated as an error, because the whole control plane starts
at once (#58).

## Ports and endpoints

| binary | port | protocol | paths |
|---|---|---|---|
| kube-apiserver | `--secure-port` (6443) | HTTPS | the API; `/healthz`, `/livez`, `/readyz`, `/version`, `/metrics` |
| kube-controller-manager | 10257, fixed | plain HTTP on `0.0.0.0` | `/metrics`, `/healthz` |
| kube-scheduler | 10259, fixed | plain HTTP on `0.0.0.0` | `/metrics`, `/healthz` |

The apiserver's `/metrics` is served **without authentication** (#90), and
the other two are plain HTTP with no authentication at all. Metric names
follow upstream's; the list, and where they differ from upstream, is in
[docs/metrics.md](docs/metrics.md).

## Build and test

Builds and tests run on the build box, `dev.g8.lo`, never on a workstation and
never as root there. Push first, then:

```bash
sc-build                              # cargo build && cargo test, at the pushed commit
sc-build 'cargo test -p apiserver'    # any command at the repo root
```

`sc-build` fetches the pushed commit into a scratch directory as the
`stormbuild` user, runs the command, and deletes the checkout. A failing build
is filed as a `build-failure` issue here.

Release artifacts — static musl binaries and `FROM scratch` images — are
described in [docs/releasing.md](docs/releasing.md).

## How it ships

In stormcos each binary is its own **stormd golden**: a container whose PID 1
is stormd, which runs the binary from a config baked into the golden. The
goldens are `rustkube-apiserver`, `rustkube-controller-manager` and
`rustkube-scheduler`, started by stormpump from `/etc/stormpump/boot.d/30-kube`
on the `sno` and `storage` profiles. stormcos's `deploy/build-goldens.sh`
builds the binaries **from source** at a pinned commit
(`cargo build --release --target x86_64-unknown-linux-musl`) rather than from
a release.

The **component golden** — what `stormcentral component build rustkube`
produces, `golden-rustkube-<digest>`, and what a stormcos release request
names — carries the three binaries; the three stormd goldens above are
assembled around them.

What stormcos passes today:

- **kube-apiserver:** `--etcd-servers http://127.0.0.1:2379` (plaintext,
  loopback), `--bind-addr 0.0.0.0`, `--advertise-address ${NODE_IP}`, the
  serving pair `/data/stormcert/apiserver.{crt,key}`, the ServiceAccount pair
  `/data/stormcert/sa-token.{key,pub}`, and
  `--manifest-dir /etc/kubernetes/manifests.d` (Cilium, CoreDNS, the storage
  class and CSI driver, the VMI CRD). The `sno` profile adds
  `--dev-anonymous-admin true`. Liveness is `https://127.0.0.1:6443/healthz`.
- **kube-controller-manager, kube-scheduler:**
  `--apiserver https://${NODE_IP}:6443 --certificate-authority /data/stormcert/ca.crt`
  and **no client credential**, so they reach the apiserver as anonymous.
  That works only where anonymous is `cluster-admin`, i.e. `sno`; on
  `storage` they start and are refused (stormcos#76).

The files under `/data/stormcert` are written by stormcert before the
apiserver starts: `apiserver.crt`/`.key` (CN `apiserver`; SANs the
`kubernetes…` names, `localhost`, `10.96.0.1`, the node IP and `127.0.0.1`),
`ca.crt`, `sa-token.key`/`.pub` (RSA-3072), and `node-admin.token`.

## Further reading

| | |
|---|---|
| [docs/presentation.md](docs/presentation.md) | a 12-slide overview (Marp): purpose, place in stormcos, what works, what is planned |
| [docs/certificates.md](docs/certificates.md) | TLS, reload, renewal, offline-minted tokens |
| [docs/storage.md](docs/storage.md) | who does what to a PVC — rustkube, stormblock, stormblock-csi |
| [docs/metrics.md](docs/metrics.md) | every metric and what it answers |
| [docs/releasing.md](docs/releasing.md) | release artifacts |
| [docs/oc-compatibility.md](docs/oc-compatibility.md) | what `oc` can ask, and what is answered |
| [docs/upstream-feature-inventory.md](docs/upstream-feature-inventory.md) | upstream feature by feature |
| [docs/scheduler-research.md](docs/scheduler-research.md), [docs/scheduler-upstream.md](docs/scheduler-upstream.md), [docs/draining-multiarch-research.md](docs/draining-multiarch-research.md) | research on upstream — design input, not a description of this code |
| [CHANGELOG.md](CHANGELOG.md) | what changed, by release |

## License

Apache-2.0
