#!/usr/bin/env bash
#
# The --manifest-dir applier prunes what left the directory (#236), against a
# real apiserver on fastetcd restarted over the same store: stormcos's
# network flavour switch (stormcos#668), where the other flavour's files are
# links into an unmounted pallet (#261).
#
# First boot (cilium): a DaemonSet with the mode as a label (#299), a
# ServiceAccount and a ConfigMap with it as an annotation, an EnsureExists
# ConfigMap, and a ClusterRole; flowsdn's file is a dangling link.
# Second boot (flowsdn): cilium's files dangle, flowsdn's is there; an
# operator took the ClusterRole off Reconcile before it.
# - cilium's DaemonSet, ServiceAccount and Reconcile ConfigMap are gone
# - the EnsureExists ConfigMap and the taken-over ClusterRole stay
# - flowsdn's DaemonSet is there; each prune is logged
# Third boot (unchanged): nothing pruned.
#
# Exit status is the number of failed checks.
MD=$(mktemp -d "${TMPDIR:-/tmp}/rk-prune.XXXXXX")
PAL=$(mktemp -d "${TMPDIR:-/tmp}/rk-pallets.XXXXXX")
mkdir -p "$PAL/cilium" "$PAL/flowsdn"
cat >"$PAL/cilium/60-cilium.yaml" <<'Y'
apiVersion: v1
kind: ServiceAccount
metadata:
  name: cilium
  namespace: kube-system
  annotations: {addonmanager.kubernetes.io/mode: Reconcile}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: cilium-config
  namespace: kube-system
  annotations: {addonmanager.kubernetes.io/mode: Reconcile}
data: {a: "1"}
---
apiVersion: v1
kind: ConfigMap
metadata: {name: cilium-seed, namespace: kube-system}
data: {a: "1"}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: cilium-taken
  labels: {addonmanager.kubernetes.io/mode: Reconcile}
rules: []
---
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: cilium
  namespace: kube-system
  labels: {addonmanager.kubernetes.io/mode: Reconcile}
spec:
  selector: {matchLabels: {k8s-app: cilium}}
  template:
    metadata: {labels: {k8s-app: cilium}}
    spec: {containers: [{name: agent, image: cilium}]}
Y
cat >"$PAL/flowsdn/60-flowsdn.yaml" <<'Y'
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: flowsdn
  namespace: kube-system
  labels: {addonmanager.kubernetes.io/mode: Reconcile}
spec:
  selector: {matchLabels: {k8s-app: flowsdn}}
  template:
    metadata: {labels: {k8s-app: flowsdn}}
    spec: {containers: [{name: agent, image: flowsdn}]}
Y
ln -s "$PAL/cilium/60-cilium.yaml" "$MD/60-cilium.yaml"
ln -s "$PAL/flowsdn-unmounted/60-flowsdn.yaml" "$MD/60-flowsdn.yaml"
RK_APISERVER_ARGS="--manifest-dir $MD"
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
code() { curl -sk -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $ADMIN" "$API$1"; }
want() { # want <code> <path> <what>
  local got; got=$(code "$2")
  [ "$got" = "$1" ] && pass "$3 ($got)" || fail "$3 (want $1, got $got)"
}
restart() {
  kill "$API_PID"; wait "$API_PID" 2>/dev/null
  start_apiserver 0; API_PID=${API_PIDS[0]}
  for _ in $(seq 120); do
    curl -sfk -H "Authorization: Bearer $ADMIN" "$API/readyz" >/dev/null && break
    sleep 1
  done
}
KS=/api/v1/namespaces/kube-system
want 200 /apis/apps/v1/namespaces/kube-system/daemonsets/cilium "boot 1: cilium DaemonSet applied"
want 404 /apis/apps/v1/namespaces/kube-system/daemonsets/flowsdn "boot 1: flowsdn's dangling link applies nothing"
want 200 $KS/configmaps/rustkube-manifests "boot 1: inventory recorded"

# The flavour switch: cilium's pallet unmounted, flowsdn's mounted.
curl -sk -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/merge-patch+json' -X PATCH \
  -d '{"metadata":{"labels":{"addonmanager.kubernetes.io/mode":"EnsureExists"}}}' \
  "$API/apis/rbac.authorization.k8s.io/v1/clusterroles/cilium-taken" >/dev/null
mv "$PAL/cilium" "$PAL/cilium-unmounted"
mv "$PAL/flowsdn" "$PAL/flowsdn-unmounted"
restart
want 404 /apis/apps/v1/namespaces/kube-system/daemonsets/cilium "boot 2: cilium DaemonSet (mode as a label) pruned"
want 404 $KS/serviceaccounts/cilium "boot 2: cilium ServiceAccount pruned"
want 404 $KS/configmaps/cilium-config "boot 2: cilium Reconcile ConfigMap pruned"
want 200 $KS/configmaps/cilium-seed "boot 2: EnsureExists ConfigMap left"
want 200 /apis/rbac.authorization.k8s.io/v1/clusterroles/cilium-taken "boot 2: ClusterRole taken off Reconcile left"
want 200 /apis/apps/v1/namespaces/kube-system/daemonsets/flowsdn "boot 2: flowsdn DaemonSet applied"
grep -q 'pruned DaemonSet kube-system/cilium: no manifest' "$W/apiserver.log" \
  && pass "boot 2: the prune is logged" || fail "boot 2: the prune is logged"
before=$(grep -c 'manifests: pruned ' "$W/apiserver.log")

restart
want 200 /apis/apps/v1/namespaces/kube-system/daemonsets/flowsdn "boot 3: flowsdn DaemonSet still there"
after=$(grep -c 'manifests: pruned ' "$W/apiserver.log")
[ "$after" = "$before" ] && pass "boot 3: nothing pruned ($before prune lines)" || fail "boot 3: pruned again ($before → $after)"
rm -rf "$MD" "$PAL"
report
