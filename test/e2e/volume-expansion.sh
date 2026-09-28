#!/usr/bin/env bash
#
# Volume expansion against this apiserver (#63): the real CSI hostpath
# driver, external-provisioner and external-resizer, on a real apiserver and
# controller-manager on fastetcd.
#
#   test/e2e/volume-expansion.sh        # from the checkout; builds what it runs
#
# Growing a claim is split the way the rest of storage is: the apiserver
# admits the larger request (only for a Bound claim in a class that allows
# expansion), the driver's external-resizer does the growing and writes the
# claim's resize status, and the kubelet grows the filesystem. There is no
# kubelet here, so the claim must stop at FileSystemResizePending with its
# old capacity — which is the handshake rustkube's binder used to break by
# copying the volume's new capacity onto the claim.
#
# Needs podman (to extract the three binaries from their release images) and
# network access to registry.k8s.io. Exit status is the number of failed
# checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

HOSTPATH=${HOSTPATH_VERSION:-v1.17.1}
PROVISIONER=${PROVISIONER_VERSION:-v6.3.0}
# v2.3.0 is released on GitHub but has no image on registry.k8s.io yet.
RESIZER=${RESIZER_VERSION:-v2.2.0}
DRIVER=hostpath.csi.k8s.io
start_controller_manager

KUBECTL=$(command -v kubectl || true)
[ -n "$KUBECTL" ] || { echo "no kubectl"; exit 100; }
cat >"$W/admin.kubeconfig" <<KC
apiVersion: v1
kind: Config
clusters: [ { name: rk, cluster: { server: "$API", certificate-authority: "$W/ca.crt" } } ]
users: [ { name: u, user: { token: "$ADMIN" } } ]
contexts: [ { name: rk, context: { cluster: rk, user: u } } ]
current-context: rk
KC
k() { "$KUBECTL" --kubeconfig "$W/admin.kubeconfig" "$@"; }
logs() { for f in hostpath provisioner resizer; do echo "---- $f log (tail)"; tail -25 "$W/$f.log" 2>/dev/null; done; }
trap '[ "$FAIL" -ne 0 ] && logs; cleanup; rm -rf "$DATA"' EXIT

extract() { # <image> <path in image> <out>
  podman pull -q "$1" >/dev/null && cid=$(podman create "$1") && podman cp "$cid:$2" "$3" && podman rm -f "$cid" >/dev/null \
    || { echo "cannot extract $2 from $1"; exit 100; }
}
extract registry.k8s.io/sig-storage/hostpathplugin:$HOSTPATH /hostpathplugin "$W/hostpathplugin"
extract registry.k8s.io/sig-storage/csi-provisioner:$PROVISIONER /csi-provisioner "$W/csi-provisioner"
extract registry.k8s.io/sig-storage/csi-resizer:$RESIZER /csi-resizer "$W/csi-resizer"
echo "hostpathplugin $HOSTPATH, csi-provisioner $PROVISIONER, csi-resizer $RESIZER"

# The driver, on a socket, with its state in the scratch dir.
sock=$W/csi.sock
mkdir -p "$W/csi-data"
"$W/hostpathplugin" --drivername=$DRIVER --endpoint="unix://$sock" --nodeid=node-a \
  --statedir="$W/csi-data" --v=5 >"$W/hostpath.log" 2>&1 &
for _ in $(seq 30); do [ -S "$sock" ] && break; sleep 1; done
[ -S "$sock" ] && pass "hostpath driver listening" || { fail "hostpath driver did not start"; report; }

# The sidecars, beside it, as the Deployment runs them.
"$W/csi-provisioner" --csi-address="$sock" --kubeconfig="$W/admin.kubeconfig" --leader-election=false \
  --feature-gates=Topology=false --v=5 >"$W/provisioner.log" 2>&1 &
"$W/csi-resizer" --csi-address="$sock" --kubeconfig="$W/admin.kubeconfig" --leader-election=false \
  --v=5 >"$W/resizer.log" 2>&1 &

cat <<Y | k apply -f - >"$W/out" 2>&1 || fail "apply classes/claim: $(cat "$W/out")"
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata: { name: grows }
provisioner: $DRIVER
allowVolumeExpansion: true
volumeBindingMode: Immediate
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata: { name: fixed }
provisioner: $DRIVER
volumeBindingMode: Immediate
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: grow-me, namespace: default }
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: grows
  resources: { requests: { storage: 1Gi } }
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: stay-small, namespace: default }
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: fixed
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
for c in grow-me stay-small; do
  waitj 90 -n default pvc $c -- '{.status.phase}' Bound >/dev/null \
    && pass "$c provisioned by the external-provisioner and Bound" \
    || { fail "$c not Bound: $(k -n default get pvc $c -o jsonpath='{.status} {.metadata.annotations}')"; report; }
done
pv=$(k -n default get pvc grow-me -o jsonpath='{.spec.volumeName}')
[ "$(k -n default get pvc grow-me -o jsonpath='{.status.capacity.storage}')" = 1Gi ] \
  && pass "grow-me has 1Gi" || fail "grow-me capacity: $(k -n default get pvc grow-me -o jsonpath='{.status.capacity}')"

# Admission: what may not change.
out=$(k -n default patch pvc stay-small --type=merge -p '{"spec":{"resources":{"requests":{"storage":"2Gi"}}}}' 2>&1)
echo "$out" | grep -q "storageclass that provisions the pvc must support resize" \
  && pass "growing a claim whose class does not allow expansion is refused" || fail "stay-small grew: $out"
out=$(k -n default patch pvc grow-me --type=merge -p '{"spec":{"resources":{"requests":{"storage":"512Mi"}}}}' 2>&1)
echo "$out" | grep -q "can not be less than status.capacity" \
  && pass "shrinking below status.capacity is refused" || fail "grow-me shrank: $out"
out=$(k -n default patch pvc grow-me --type=merge -p '{"spec":{"accessModes":["ReadWriteMany"]}}' 2>&1)
echo "$out" | grep -q "spec is immutable" \
  && pass "a Bound claim's access modes are immutable" || fail "grow-me access modes changed: $out"

# Grow it.
k -n default patch pvc grow-me --type=merge -p '{"spec":{"resources":{"requests":{"storage":"2Gi"}}}}' >"$W/out" 2>&1 \
  && pass "grow-me asked for 2Gi" || fail "grow: $(cat "$W/out")"
waitj 90 pv "$pv" -- '{.spec.capacity.storage}' 2Gi >/dev/null \
  && pass "the external-resizer grew the volume to 2Gi" || fail "PV capacity: $(k get pv "$pv" -o jsonpath='{.spec.capacity}')"
cond() { k -n default get pvc grow-me -o jsonpath='{range .status.conditions[*]}{.type}={.status} {end}'; }
for _ in $(seq 60); do cond | grep -q "FileSystemResizePending=True" && break; sleep 1; done
if cond | grep -q "FileSystemResizePending=True"; then
  pass "grow-me waits for the node: FileSystemResizePending"
else
  # A driver that needs no node step finishes here instead.
  [ "$(k -n default get pvc grow-me -o jsonpath='{.status.capacity.storage}')" = 2Gi ] \
    && pass "grow-me has 2Gi (no node expansion needed)" || fail "grow-me conditions: $(cond); status $(k -n default get pvc grow-me -o jsonpath='{.status}')"
fi
[ "$(k -n default get pvc grow-me -o jsonpath='{.status.allocatedResources.storage}')" = 2Gi ] \
  && pass "status.allocatedResources is 2Gi" || fail "allocatedResources: $(k -n default get pvc grow-me -o jsonpath='{.status.allocatedResources}')"
# The handshake holds: while the node step is pending, the claim keeps its
# old capacity and its conditions — nothing in the control plane overwrites
# them (the binder did, every pass).
if cond | grep -q "FileSystemResizePending=True"; then
  sleep 20
  got="$(k -n default get pvc grow-me -o jsonpath='{.status.capacity.storage}') $(cond)"
  [ "$got" = "1Gi FileSystemResizePending=True " ] \
    && pass "20 s on: still 1Gi and FileSystemResizePending (the kubelet's step)" || fail "status changed under the resize: '$got'"
fi
# And a Resizing claim is not shrunk below what it has.
out=$(k -n default patch pvc grow-me --type=merge -p '{"spec":{"resources":{"requests":{"storage":"1Gi"}}}}' 2>&1) \
  && pass "back to 1Gi (= status.capacity) is allowed: recovery" || fail "recovery refused: $out"
report
