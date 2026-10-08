# Storage — who does what to a PersistentVolumeClaim

RustKube serves the storage API and decides *bindings*. It does not create
volume bytes. Those come from StormBlock, by one of two paths:

- **The `stormblock` class — the built-in driver, and the normal case.** The
  kubelet (rustkube-node) makes the volume at pod start: a copy-on-write clone
  of a sealed, pre-formatted blank of the claim's size class, through
  stormblock's own API, attached over ublk. No CSI driver is involved; the
  control plane writes the pre-bound PersistentVolume
  ([below](#the-stormblock-class-the-built-in-pvc-driver)).
- **A CSI class** — stormblock-csi, or any third-party driver. The control
  plane hands the claim to the driver's `external-provisioner` sidecar by
  annotation and never calls the driver itself ([the chain](#the-chain-end-to-end)).

This document describes the contract between these components, so that a change on one
side is made against what the other side actually does.

## Component ownership

| repo | what it is | what it owns |
|---|---|---|
| **rustkube** (here) | control plane | binding, phases, protection finalizers, attach objects, placement |
| **stormblock** | the engine | slabs, goldens, clones, NVMe-oF/TCP export, filesystem templates |
| **stormblock-registry** (sbregistry) | the registry/orchestrator | golden volume per digest; CoW clones for container roots and PXE/firmware boots; cuts and names the PVC blank ladder (#112) |
| **rustkube-node** | kubelet with the built-in PVC driver | clones PVC blanks directly through stormblock, attaches via ublk, reclaims node-owned clones |
| **stormblock-csi** | the CSI driver + wander operator | `CreateVolume`/`DeleteVolume`, stage/publish, capacity publishing, replica placement |

## The chain, end to end

A pod asks for 20Gi under a CSI class — stormblock-csi ships one
(`deploy/02-storageclass.yaml`: class `stormblock-csi`, `provisioner:
csi.stormblock.io`, `volumeBindingMode: WaitForFirstConsumer`, not the
default). The name `stormblock` belongs to the in-kubelet class below, which
stays the default; see [class selection](#class-selection-is-by-name).

1. **The claim is created.** `persistentvolume.rs` gives it the default class,
   adds the `kubernetes.io/pvc-protection` finalizer, and — because the class
   is `WaitForFirstConsumer` — writes nothing else. The claim stays `Pending`
   with a `WaitForFirstConsumer` event. This is deliberate: the volume is going
   to be a clone made *local to the node the pod lands on*, so provisioning
   before placement would put it in the wrong place.
2. **The scheduler places the pod.** `scheduler::volumebinding` filters nodes
   on published `CSIStorageCapacity` (the wander operator publishes it, and
   `CSIDriver.spec.storageCapacity: true` is what makes the check apply),
   picks one, and writes `volume.kubernetes.io/selected-node` on the claim.
   (It does this for any unbound claim the pod uses, not only
   `WaitForFirstConsumer` ones.)
   **The pod is not bound yet** — binding a pod before its volume exists is
   how a kubelet ends up waiting on a mount that cannot succeed.
3. **The hand-off.** With a selected node present, `persistentvolume.rs` sets
   `volume.kubernetes.io/storage-provisioner: csi.stormblock.io` (and the beta
   spelling, which older sidecars still read). That annotation is the *entire*
   signal the upstream `external-provisioner` sidecar acts on. Nothing else in
   rustkube talks to StormBlock.
4. **The driver provisions.** stormblock-csi's `CreateVolume` asks the engine
   for a volume (`POST /v1/volumes`): an **empty** thin volume, which the node
   plugin formats (`mkfs.<fstype>`) the first time it stages it — or, when the
   claim has a `dataSource` (a snapshot or another volume), a CoW clone of
   that source. It does not clone a blank template; that is the in-kubelet
   path's trick, below. The sidecar then creates the PV with
   `spec.csi.volumeHandle`, `claimRef` pointing back at the claim, and
   `pv.kubernetes.io/provisioned-by`.
5. **The bind completes.** `persistentvolume.rs` sees the pre-bound PV, fills
   in `spec.volumeName` on the claim, sets both phases to `Bound`, and copies
   the real capacity into `status`. Claims are worked on in parallel (8
   workers); only a claim that must *choose* among unclaimed volumes takes the
   binder's lock, reading the volumes inside it so two claims never take one.
   A pre-bound (provisioned) volume is no choice, so a burst of new claims no
   longer binds one after another (#147: 25 claims took 0–2.6 s in a line).
6. **The attach.** `csi.stormblock.io` declares `attachRequired: true`, so
   `attachdetach.rs` creates a `VolumeAttachment` named exactly as upstream
   names it (`csi-` + SHA-256 of handle+driver+node). The `external-attacher`
   sidecar turns that into `ControllerPublishVolume`; the kubelet stages and
   publishes; the pod runs.
7. **Teardown.** Deleting the pod deletes the `VolumeAttachment`, and the
   sidecar's finalizer holds it until the driver has unpublished. Deleting a
   claim a pod still mounts is accepted but held by the `pvc-protection`
   finalizer (with a `VolumeInUse` event) until the pod is gone; then the PV
   is released. With `reclaimPolicy: Delete` the *driver* deletes the
   volume — rustkube only marks the PV `Released` and says whose job the rest
   is, because deleting the API object here would strand the clone on the
   array.

## The `stormblock` class: the built-in PVC driver

Everything above describes a **foreign** class — a third-party CSI driver —
and it is still exactly right for one. Our own storage is not a foreign
class: stormcos has a **built-in PVC driver**, and the `stormblock` class is
it.

The driver is built on stormblock and sbregistry's **blanks**, for speed: a
blank is a sealed, pre-formatted ext4 volume of a size class, named
`pvc-ext4j-<MiB>m` (`pvc-ext4j-1m` … `pvc-ext4j-1024m` ship in the stormcos
image; `template_name` in rustkube-node's `storage.rs`), so
provisioning a claim is a copy-on-write clone of one — no mkfs, no copying, no
round trip through a provisioner. The kubelet does it at pod start
(`rustkube-node`, `pkg/kubelet/src/storage.rs`): the claim is rounded up to a
size class, the blank is CoW-cloned through stormblock's own API, attached over
ublk, and handed to stormpump with `fstype: ext4` for the container to mount
in its own namespace. No CSI driver, no sidecars, and no host-side mount to
propagate — so none of the CSI path's overhead.

CSI support (the rest of this document, the kubelet's CSI node client
rustkube-node#52, stormpump#35's mount propagation) exists only for
third-party drivers. It is never on the path of a `stormblock` claim.

So the split for that one class is:

| | |
|---|---|
| clone, attach, mount | the kubelet, at pod start |
| the `PersistentVolume`, pre-bound to the claim | `controller-manager/src/stormblock.rs` (#71) |
| binding and phases | `persistentvolume.rs`, as for any class |
| delete the clone on a `Released`/`Delete` PV, then the PV | the kubelet on the node holding it (`reclaim_released`, rustkube-node#46) |
| **the name that joins them** | `pvc-<ns>-<claim>`, derived on both sides |

`stormblock.rs` writes the PV only once the scheduler has written
`volume.kubernetes.io/selected-node` (the claim is `WaitForFirstConsumer`),
with a hostname `nodeAffinity` for that node, `provisioner`
`stormblock.storm.io/in-kubelet` and CSI driver `stormblock.storm.io`. Both
orders converge: the kubelet may provision before the controller writes the
object or after it, and neither is an error.

This path relies on stormcos shipping `CSIDriver stormblock.storm.io` with
`attachRequired: false` (stormcos `deploy/manifests/46-csidriver.yaml`).
`attachdetach.rs` treats a driver with no CSIDriver object as needing
attachment, so without that object every such PV would get a
`VolumeAttachment` that nothing ever acts on. Until `stormblock.rs` writes the
PV, `persistentvolume.rs` also sets the storage-provisioner annotation and an
`ExternalProvisioning` event on the claim; both are transient.

### Class selection is by name

`stormblock.rs` selects claims by the class **name** `stormblock` and never
reads the class's provisioner. stormcos's manifest
(`deploy/manifests/45-storageclass.yaml`) defines `stormblock` with
provisioner `stormblock.storm.io` (this path). stormblock-csi names its
StorageClass `stormblock-csi` (stormblock-csi@931b085), so the two no longer
collide; only its VolumeSnapshotClass and VolumeGroupSnapshotClass are named
`stormblock`, and they are not StorageClasses. What remains is #92: a
StorageClass named `stormblock` with any other provisioner would still be
provisioned by this path.

**Deleting the backing clone is the node's job.** stormblock's management API
defaults to `0.0.0.0:9090`, not loopback. It authenticates with a per-node
bearer token (stormblock#107); a token minted on one node grants no authority
on a peer (#117). With that node's credentials, the kubelet's
`reclaim_released` deletes the clone and then the PV (rustkube-node#46). The
control plane only reports that the node will.

## Generic ephemeral volumes

A Pod volume with `ephemeral.volumeClaimTemplate` gets its claim from the
controller-manager's ephemeral-volume controller (#94), as upstream's:
`<pod>-<volume>` in the Pod's namespace, the template's labels, annotations
and spec, owned by the Pod (`controller: true`), so the garbage collector
deletes it with the Pod. From there it is an ordinary claim — bound by the PV
binder, placed by the scheduler (which looks it up by that name), mounted by
the kubelet. A claim of that name the Pod does not own is never adopted: the
Pod gets a Warning Event and waits.

## Volume expansion

Growing a claim is three parties, and rustkube is the first (#63):

1. **the apiserver** admits the larger request: only on a Bound claim, only
   `resources.requests.storage`, only if the claim's StorageClass has
   `allowVolumeExpansion: true` (403 otherwise, upstream's
   `PersistentVolumeClaimResize`). A request may shrink only back to
   `status.capacity`, the recovery from an expansion the driver could not
   make; the rest of a claim's spec is immutable (422);
2. **the driver's external-resizer** sees the request above `status.capacity`,
   calls `ControllerExpandVolume`, grows the PV's `spec.capacity`, and writes
   the claim's resize status itself: `allocatedResources`,
   `allocatedResourceStatuses`, the `Resizing` condition and, when the node
   has to grow the filesystem, `FileSystemResizePending`;
3. **the kubelet** grows the filesystem (`NodeExpandVolume`), sets the
   claim's `status.capacity` and clears the conditions — rustkube-node#42.

The binder sets a claim's `status.capacity` as it becomes Bound and then
leaves it: between steps 2 and 3 the volume is larger than the filesystem,
and the claim says so. (It used to copy the PV's capacity onto the claim on
every pass, which wiped the handshake.)

`test/e2e/volume-expansion.sh` runs the real hostpath CSI driver,
external-provisioner and external-resizer against a real control plane:
a claim provisioned, the three refusals, a grow to 2Gi that stops — with no
kubelet — at `FileSystemResizePending` with the old capacity, and the
recovery shrink. Against the code before #63 it fails four checks.

The in-kubelet `stormblock` class has no resizer; expanding it is the
kubelet's business when it is wanted.

## Volume snapshots

`VolumeSnapshot`, `VolumeSnapshotContent` and `VolumeSnapshotClass` are not
built into rustkube. They are CRDs from `kubernetes-csi/external-snapshotter`,
served like any other custom resource, and three things outside this repo
make them work:

- **the CRDs and the snapshot-controller** (one per cluster, watching every
  namespace) are the cluster's business: installing them is stormcos#170
  (moved from stormpump#28 with `deploy/manifests/`), still open — they are
  not in stormcos's manifests today;
- **the `csi-snapshotter` sidecar** runs beside a CSI driver and calls its
  `CreateSnapshot`/`DeleteSnapshot`: stormblock-csi's business. It implements
  them on stormblock's `/v1/snapshots`, which makes a CoW snapshot volume, so
  snapshots map onto stormblock's clones (stormblock#111, answered and closed);
- the in-kubelet `stormblock` class has no sidecars at all, so snapshots are
  for CSI-class claims.

What rustkube owes is that upstream's controller works against this
apiserver, and `test/e2e/snapshot-controller.sh` checks exactly that
(#64): it applies external-snapshotter v8.6.0's six CRDs with kubectl, runs
the real snapshot-controller from its release image as its own ServiceAccount
with upstream's RBAC and leader election, binds a PVC to a CSI PV, snapshots
it, and plays the sidecar (content status on create, the finalizer on
delete). Create → bind → ready → delete passes end to end. Getting there fixed
four rustkube bugs: an unqualified ServiceAccount in a RoleBinding was read as
`default`'s (the controller could not take its Lease), a claim bound by
`volumeName` never bound its volume (the controller refused it), and the
protobuf codec dropped Go's inline embeds (a PV read by client-go had no
`csi`) and the webhook configurations' `Webhooks` field.

## What rustkube deliberately does not do

- **It provisions nothing but the `stormblock` class.** A class with
  `kubernetes.io/no-provisioner` binds statically against PVs an administrator
  created; everything else is handed to the driver named by the class. The
  kubelet draws the same line, on the same class name. That keeps the kubelet
  and this controller from both owning a claim, but not this controller and a
  StorageClass named `stormblock` with another provisioner (#92); no shipped
  manifest defines one today.
- **It never deletes a backing volume.** A `Delete` PV with no
  `pv.kubernetes.io/provisioned-by` gets a `VolumeFailedDelete` warning and
  stays `Released`, which is the truth, rather than a phase that implies
  something is happening.
- **Generic binding and attachment use Kubernetes storage objects.**
  Third-party CSI drivers use their sidecars. The `stormblock` class is an
  explicit built-in special case in `stormblock.rs`; its node driver clones
  blanks directly and never calls sbregistry to clone the PVC.

## Cross-checks that matter

These are the places where a change on one side silently breaks the other:

| if this changes | check |
|---|---|
| the provisioner name in `stormblock-csi/deploy/02-storageclass.yaml` | nothing in `persistentvolume.rs`, which reads the class; but `stormblock.rs` hard-codes the class name `stormblock`, the provisioner `stormblock.storm.io/in-kubelet` and the driver `stormblock.storm.io` |
| `attachRequired` on the CSIDriver | `attachdetach.rs` stops creating attachments; the driver must not wait for one |
| `storageCapacity` on the CSIDriver | the scheduler's capacity filter switches on or off; with it on and nothing published, every node is refused |
| the `CSIStorageCapacity` topology labels | `volumebinding.rs` matches them as a `LabelSelector` against node labels |
| the annotation names upstream uses | both spellings of `storage-provisioner` are written; the sidecar reads either |

## Testing

`stormblock-csi` boots a real rustkube apiserver in its e2e suite
(`make e2e`, `KUBE_APISERVER_BIN` points at this repo's binary), which is the
integration test for this contract. On this side, the binder's matching rules
are unit-tested in `pkg/controller-manager/src/persistentvolume.rs` and the
placement rules in `pkg/scheduler/src/volumebinding.rs`.
`test/e2e/snapshot-controller.sh` runs upstream's snapshot-controller against
a real control plane on fastetcd (see Volume snapshots above). `test/e2e/volume-expansion.sh` does the same for the
external-provisioner and external-resizer with the hostpath CSI driver.
