#!/usr/bin/env bash
#
# Volume snapshots against this apiserver (#64): upstream's external-snapshotter
# CRDs and its real snapshot-controller, on a real apiserver and
# controller-manager on fastetcd.
#
#   test/e2e/snapshot-controller.sh        # from the checkout; builds what it runs
#
# What rustkube owes snapshots (installing them is stormpump#28, a driver
# that snapshots is stormblock-csi's) is that they work against this
# apiserver: the CRDs establish, their /status subresources work, and a Go
# controller watching them cluster-wide as a ServiceAccount — the shape of
# client that found every informer bug so far — does its job. So:
#
# - applies the six CRDs of external-snapshotter $SNAP_VERSION with kubectl
#   and waits for Established;
# - applies upstream's RBAC and runs upstream's snapshot-controller binary
#   (extracted from its release image) as system:serviceaccount:kube-system:
#   snapshot-controller, with leader election, as its Deployment does;
# - binds a PVC to a CSI PV, snapshots it, and plays the csi-snapshotter
#   sidecar (which belongs beside a driver and is absent here): it writes the
#   VolumeSnapshotContent's status, and on delete drops its finalizer;
# - checks the controller's side at each step: content created and bound,
#   PVC source protection added and released, VolumeSnapshot ready, and a
#   delete that takes the content with it.
#
# Needs podman (to extract the binary) and network access to GitHub and
# registry.k8s.io. Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

SNAP_VERSION=${SNAP_VERSION:-v8.6.0}
RAW=https://raw.githubusercontent.com/kubernetes-csi/external-snapshotter/$SNAP_VERSION
start_controller_manager

# On any failure, what the controller said.
sc_log() { [ -f "$W/sc.log" ] && { echo "---- snapshot-controller log (head 40, tail 60)"; head -40 "$W/sc.log"; echo "…"; tail -60 "$W/sc.log"; }; }
trap 'sc_log_on_fail' EXIT
# Replaces lib.sh's trap, so it does what that one did: stop the rig, and
# remove the store (on tmpfs) and the scratch dir.
sc_log_on_fail() { [ "$FAIL" -ne 0 ] && sc_log; cleanup; rm -rf "$DATA"; }

# --- tools -----------------------------------------------------------------------
KUBECTL=$(command -v kubectl || true)
if [ -z "$KUBECTL" ]; then
  KUBECTL=$W/kubectl
  curl -sfL -o "$KUBECTL" "https://dl.k8s.io/$(curl -sfL https://dl.k8s.io/release/stable-1.36.txt)/bin/linux/amd64/kubectl" || exit 100
  chmod +x "$KUBECTL"
fi
kc() { # <kubeconfig> <server token>
  cat >"$1" <<KC
apiVersion: v1
kind: Config
clusters: [ { name: rk, cluster: { server: "$API", certificate-authority: "$W/ca.crt" } } ]
users: [ { name: u, user: { token: "$2" } } ]
contexts: [ { name: rk, context: { cluster: rk, user: u } } ]
current-context: rk
KC
}
kc "$W/admin.kubeconfig" "$ADMIN"
k() { "$KUBECTL" --kubeconfig "$W/admin.kubeconfig" "$@"; }

img=registry.k8s.io/sig-storage/snapshot-controller:$SNAP_VERSION
podman pull -q "$img" >/dev/null || { echo "cannot pull $img"; exit 100; }
cid=$(podman create "$img") && podman cp "$cid:/snapshot-controller" "$W/snapshot-controller" && podman rm -f "$cid" >/dev/null \
  || { echo "cannot extract the snapshot-controller from $img"; exit 100; }
echo "snapshot-controller $SNAP_VERSION, kubectl $("$KUBECTL" version --client 2>/dev/null | head -1)"

# --- CRDs --------------------------------------------------------------------------
crds="snapshot.storage.k8s.io_volumesnapshotclasses snapshot.storage.k8s.io_volumesnapshotcontents snapshot.storage.k8s.io_volumesnapshots
      groupsnapshot.storage.k8s.io_volumegroupsnapshotclasses groupsnapshot.storage.k8s.io_volumegroupsnapshotcontents groupsnapshot.storage.k8s.io_volumegroupsnapshots"
for c in $crds; do
  curl -sfL "$RAW/client/config/crd/$c.yaml" -o "$W/$c.yaml" || { echo "cannot fetch $c"; exit 100; }
  if k apply -f "$W/$c.yaml" >"$W/apply.out" 2>&1; then :; else fail "apply $c: $(cat "$W/apply.out")"; fi
done
for c in $crds; do
  name=${c#*_}.${c%%_*}
  if k wait --for condition=established --timeout=30s "crd/$name" >/dev/null 2>&1; then
    pass "CRD $name Established"
  else
    fail "CRD $name not Established: $(k get crd "$name" -o jsonpath='{.status.conditions}' 2>&1)"
  fi
done
k api-resources --api-group=snapshot.storage.k8s.io 2>&1 | grep -q volumesnapshots \
  && pass "discovery lists snapshot.storage.k8s.io" || fail "discovery: $(k api-resources --api-group=snapshot.storage.k8s.io 2>&1)"

# --- the controller, as its ServiceAccount ------------------------------------------
curl -sfL "$RAW/deploy/kubernetes/snapshot-controller/rbac-snapshot-controller.yaml" -o "$W/rbac.yaml" || exit 100
k apply -f "$W/rbac.yaml" >"$W/apply.out" 2>&1 || fail "apply RBAC: $(cat "$W/apply.out")"
kc "$W/sc.kubeconfig" "$(token system:serviceaccount:kube-system:snapshot-controller '[]')"
"$W/snapshot-controller" --kubeconfig "$W/sc.kubeconfig" --v=5 \
  --leader-election=true --leader-election-namespace=kube-system >"$W/sc.log" 2>&1 &
sc_pid=$!
lease=
for _ in $(seq 60); do
  kill -0 $sc_pid 2>/dev/null || break
  lease=$(k -n kube-system get lease snapshot-controller-leader -o jsonpath='{.spec.holderIdentity}' 2>/dev/null)
  [ -n "$lease" ] && break
  sleep 1
done
if ! kill -0 $sc_pid 2>/dev/null; then
  fail "snapshot-controller exited: $(tail -20 "$W/sc.log")"; report
fi
[ -n "$lease" ] && pass "snapshot-controller leads (Lease held by $lease)" || fail "no leader Lease after 60 s"

# --- a snapshot, end to end -----------------------------------------------------------
cat <<Y | k apply -f - >"$W/apply.out" 2>&1 || fail "apply workload: $(cat "$W/apply.out")"
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshotClass
metadata: { name: fake-class }
driver: fake.csi.storm.io
deletionPolicy: Delete
---
apiVersion: v1
kind: PersistentVolume
metadata: { name: snap-pv }
spec:
  capacity: { storage: 1Gi }
  accessModes: [ReadWriteOnce]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ""
  csi: { driver: fake.csi.storm.io, volumeHandle: vol-1 }
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: snap-src, namespace: default }
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: ""
  volumeName: snap-pv
  resources: { requests: { storage: 1Gi } }
Y
waitj() { # <seconds> <kubectl get args…> -- <jsonpath> <want>
  local n=$1; shift; local args=() want jp got
  while [ "$1" != "--" ]; do args+=("$1"); shift; done; shift; jp=$1 want=$2
  for _ in $(seq "$n"); do
    got=$(k get "${args[@]}" -o jsonpath="$jp" 2>/dev/null)
    [ "$got" = "$want" ] && return 0
    sleep 1
  done
  echo "$got"; return 1
}
waitj 60 -n default pvc snap-src -- '{.status.phase}' Bound >/dev/null \
  && pass "PVC Bound to the CSI PV" || fail "PVC not Bound: $(k -n default get pvc snap-src -o jsonpath='{.status}')"

cat <<Y | k apply -f - >"$W/apply.out" 2>&1 || fail "apply VolumeSnapshot: $(cat "$W/apply.out")"
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata: { name: snap1, namespace: default }
spec:
  volumeSnapshotClassName: fake-class
  source: { persistentVolumeClaimName: snap-src }
Y
content=
for _ in $(seq 60); do
  content=$(k -n default get volumesnapshot snap1 -o jsonpath='{.status.boundVolumeSnapshotContentName}' 2>/dev/null)
  [ -n "$content" ] && break
  sleep 1
done
if [ -z "$content" ]; then
  fail "the controller bound no VolumeSnapshotContent in 60 s: $(k -n default get volumesnapshot snap1 -o jsonpath='{.status}')"
  report
fi
pass "VolumeSnapshot bound to $content"
got=$(k get volumesnapshotcontent "$content" -o jsonpath='{.spec.driver} {.spec.source.volumeHandle} {.spec.volumeSnapshotRef.name} {.spec.deletionPolicy}')
[ "$got" = "fake.csi.storm.io vol-1 snap1 Delete" ] && pass "content: driver, volumeHandle, snapshot ref, policy" \
  || fail "content spec: '$got'"
k -n default get pvc snap-src -o jsonpath='{.metadata.finalizers}' | grep -q pvc-as-source-protection \
  && pass "PVC carries source protection while the snapshot is taken" \
  || fail "PVC finalizers: $(k -n default get pvc snap-src -o jsonpath='{.metadata.finalizers}')"

# The csi-snapshotter sidecar's half: the snapshot is cut and ready.
now_ns=$(($(date +%s) * 1000000000))
k patch volumesnapshotcontent "$content" --subresource=status --type=merge \
  -p "{\"status\":{\"snapshotHandle\":\"snap-handle-1\",\"readyToUse\":true,\"restoreSize\":1073741824,\"creationTime\":$now_ns}}" \
  >"$W/apply.out" 2>&1 && pass "content /status written (as the sidecar would)" || fail "content /status: $(cat "$W/apply.out")"
if waitj 60 -n default volumesnapshot snap1 -- '{.status.readyToUse}' true >/dev/null; then
  pass "VolumeSnapshot readyToUse, restoreSize $(k -n default get volumesnapshot snap1 -o jsonpath='{.status.restoreSize}')"
else
  fail "VolumeSnapshot not ready: $(k -n default get volumesnapshot snap1 -o jsonpath='{.status}')"
fi
for _ in $(seq 30); do
  k -n default get pvc snap-src -o jsonpath='{.metadata.finalizers}' | grep -q pvc-as-source-protection || break
  sleep 1
done
k -n default get pvc snap-src -o jsonpath='{.metadata.finalizers}' | grep -q pvc-as-source-protection \
  && fail "PVC source protection not released once the snapshot is ready" || pass "PVC source protection released"

# Delete: the snapshot goes, and with deletionPolicy Delete the content too.
# The controller marks the content being-deleted and deletes it; the sidecar
# deletes the snapshot on the storage and then drops the content's
# bound-protection finalizer; the controller then releases the VolumeSnapshot.
k -n default delete volumesnapshot snap1 --wait=false >/dev/null 2>&1
marked=
for _ in $(seq 60); do
  if [ "$(k get volumesnapshotcontent "$content" -o jsonpath='{.metadata.annotations.snapshot\.storage\.kubernetes\.io/volumesnapshot-being-deleted}' 2>/dev/null)" = yes ] \
     && [ -n "$(k get volumesnapshotcontent "$content" -o jsonpath='{.metadata.deletionTimestamp}' 2>/dev/null)" ]; then
    marked=1; break
  fi
  sleep 1
done
[ -n "$marked" ] && pass "controller marked the content being-deleted and deleted it" \
  || fail "content not marked and deleted: $(k get volumesnapshotcontent "$content" -o jsonpath='{.metadata}' 2>&1)"
k patch volumesnapshotcontent "$content" --type=json -p '[{"op":"remove","path":"/metadata/finalizers"}]' >"$W/apply.out" 2>&1 \
  && pass "content finalizer removed (as the sidecar would)" || fail "remove content finalizer: $(cat "$W/apply.out")"
gone=
for _ in $(seq 90); do
  if ! k -n default get volumesnapshot snap1 >/dev/null 2>&1 && ! k get volumesnapshotcontent "$content" >/dev/null 2>&1; then gone=1; break; fi
  sleep 1
done
[ -n "$gone" ] && pass "VolumeSnapshot and its content deleted" \
  || fail "after delete: snapshot $(k -n default get volumesnapshot snap1 -o jsonpath='{.metadata.finalizers}' 2>&1), content $(k get volumesnapshotcontent "$content" -o jsonpath='{.metadata.finalizers} {.metadata.annotations}' 2>&1)"

errs=$(grep -Ec '^E[0-9]{4}' "$W/sc.log" || true)
echo "---- snapshot-controller: $errs error lines"
grep -E '^E[0-9]{4}' "$W/sc.log" | sed 's/^.\{0,40\}\] //' | sort | uniq -c | sort -rn | head -15
report
