# Storage — who does what to a PersistentVolumeClaim

RustKube serves the storage API and decides *bindings*. It does not create
volumes. The bytes come from StormBlock, and the boundary between the two is
the whole design: a PVC in this cluster is a copy-on-write clone of a blank
filesystem template on sbregistry, made by a CSI driver that rustkube never
calls directly.

This document is the contract between the four repos, so that a change on one
side is made against what the other side actually does.

## The four pieces

| repo | what it is | what it owns |
|---|---|---|
| **rustkube** (here) | control plane | binding, phases, protection finalizers, attach objects, placement |
| **stormblock** | the engine | slabs, goldens, clones, NVMe-oF/TCP export, filesystem templates |
| **stormblock-registry** (sbregistry) | the registry/orchestrator | golden volume per digest, CoW clones for root-dirs, PVCs and PXE media |
| **stormblock-csi** | the CSI driver + wander operator | `CreateVolume`/`DeleteVolume`, stage/publish, capacity publishing, replica placement |

## The chain, end to end

A pod asks for 20Gi under a CSI class — stormblock-csi ships one
(`deploy/02-storageclass.yaml`, `provisioner: csi.stormblock.io`,
`volumeBindingMode: WaitForFirstConsumer`). That manifest names its class
`stormblock` too, which collides with the in-kubelet class below; see
[the name collision](#the-name-stormblock-is-claimed-twice).

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
4. **The driver provisions.** `stormblock-csi` calls the engine: a CoW clone of
   the blank filesystem template — a golden formatted once and sealed, so a
   new PVC costs metadata rather than a `mkfs` — and creates the PV with
   `spec.csi.volumeHandle`, `claimRef` pointing back at the claim, and
   `pv.kubernetes.io/provisioned-by`.
5. **The bind completes.** `persistentvolume.rs` sees the pre-bound PV, fills
   in `spec.volumeName` on the claim, sets both phases to `Bound`, and copies
   the real capacity into `status`.
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
blank is a sealed, pre-formatted volume of a size class (`pvc-<class>`), so
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
`attachRequired: false` (`deploy/manifests/46-csidriver.yaml`).
`attachdetach.rs` treats a driver with no CSIDriver object as needing
attachment, so without that object every such PV would get a
`VolumeAttachment` that nothing ever acts on. Until `stormblock.rs` writes the
PV, `persistentvolume.rs` also sets the storage-provisioner annotation and an
`ExternalProvisioning` event on the claim; both are transient.

### The name `stormblock` is claimed twice

`stormblock.rs` selects claims by the class **name** `stormblock` and never
reads the class's provisioner. stormcos's manifest defines `stormblock` with
provisioner `stormblock.storm.io` (this path); stormblock-csi's defines
`stormblock` with `csi.stormblock.io` (the CSI path). With the CSI manifest
applied, both would act on the same claims (#92).

**Deleting the backing clone is the node's job.** stormblock's management API
is loopback, so only the node holding a volume can delete it: the kubelet's
`reclaim_released` deletes the clone and then the PV (rustkube-node#46). The
control plane only reports that the node will.

## What rustkube deliberately does not do

- **It provisions nothing but the `stormblock` class.** A class with
  `kubernetes.io/no-provisioner` binds statically against PVs an administrator
  created; everything else is handed to the driver named by the class. The
  kubelet draws the same line, on the same class name. That keeps the kubelet
  and this controller from both owning a claim, but not this controller and a
  CSI provisioner whose class is also named `stormblock` (#92).
- **It never deletes a backing volume.** A `Delete` PV with no
  `pv.kubernetes.io/provisioned-by` gets a `VolumeFailedDelete` warning and
  stays `Released`, which is the truth, rather than a phase that implies
  something is happening.
- **It does not assume StormBlock.** Every signal above is the upstream one, so
  an OpenShift CSI driver, a vendor driver, or `hostPath` PVs behave the same.
  StormBlock is the driver we run, not a special case in the code.

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
