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

A pod asks for 20Gi under the default `stormblock` class
(`volumeBindingMode: WaitForFirstConsumer`, `provisioner: csi.stormblock.io`):

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
   sidecar's finalizer holds it until the driver has unpublished. Deleting the
   claim is refused while a pod still mounts it (`pvc-protection`), then
   releases the PV. With `reclaimPolicy: Delete` the *driver* deletes the
   volume — rustkube only marks the PV `Released` and says whose job the rest
   is, because deleting the API object here would strand the clone on the
   array.

## What rustkube deliberately does not do

- **It never provisions.** There is no in-tree provisioner and there should not
  be one. A class with `kubernetes.io/no-provisioner` binds statically against
  PVs an administrator created; everything else is handed to the driver named
  by the class.
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
| the provisioner name in `stormblock-csi/deploy/02-storageclass.yaml` | nothing in rustkube — it reads the class, never a constant |
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
