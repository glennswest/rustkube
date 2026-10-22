#!/usr/bin/env bash
#
# An apiserver that boots before its datastore answers (#293, server1 12.49's
# reboot: fastetcd repairing its data file, `0 created, 114 failed`, then 403
# for every client for ~50 s):
#
#   test/e2e/datastore-late.sh      # from the checkout; builds what it runs
#
# 1. Both stop; the apiserver starts with a --manifest-dir (a CRD, a
#    ClusterRole, a ConfigMap and a custom resource of the CRD) and the
#    datastore 150 s later — past both 60 s windows the boot used to give the
#    datastore (its wait, then the CRD load) before running the manifest pass
#    into a dead store (dfb448b7c686's apiserver: 0 created, 4 failed). The
#    apiserver waits (nothing served, an ERROR in its log), then applies
#    every manifest: 4 created, 0 failed.
# 2. With the datastore stopped under a serving apiserver, a caller with no
#    grant gets 503 + Retry-After (the refusal could not be checked), not 403;
#    once it is back, the same request is a plain 403.
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

get() { # <path> [token] — prints the HTTP code, headers in $W/hdr, body in $W/out
  curl -sk -D "$W/hdr" -o "$W/out" -w '%{http_code}' -H "Authorization: Bearer ${2:-$ADMIN}" "$API$1"
}
start_store() {
  "$FASTETCD" --data-dir "$DATA" --listen-client-urls http://127.0.0.1:$ETCD \
    --listen-peer-urls http://127.0.0.1:$((ETCD + 1)) --listen-metrics-url 127.0.0.1:$((ETCD + 2)) \
    >>"$W/fastetcd.log" 2>&1 &
  STORE_PID=$!
}

M=$W/manifests
mkdir -p "$M"
cat >"$M/10-crd.yaml" <<'EOF'
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: widgets.rk293.example.io}
spec:
  group: rk293.example.io
  scope: Cluster
  names: {plural: widgets, singular: widget, kind: Widget}
  versions:
    - name: v1
      served: true
      storage: true
      schema: {openAPIV3Schema: {type: object, x-kubernetes-preserve-unknown-fields: true}}
EOF
cat >"$M/20-objects.yaml" <<'EOF'
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata: {name: rk293-role}
rules: [{apiGroups: [""], resources: [configmaps], verbs: [get]}]
---
apiVersion: v1
kind: ConfigMap
metadata: {name: rk293, namespace: default}
data: {k: v}
---
apiVersion: rk293.example.io/v1
kind: Widget
metadata: {name: w1}
EOF

# --- 1. reboot: apiserver first, datastore 150 s later -------------------------
kill "$API_PID"; wait "$API_PID" 2>/dev/null
kill "$STORE_PID"; wait "$STORE_PID" 2>/dev/null
"$BIN/kube-apiserver" --bind-addr 127.0.0.1 --secure-port "$PORT" \
  --tls-cert-file "$W/apiserver.crt" --tls-private-key-file "$W/apiserver.key" \
  --etcd-servers "$ETCD_SERVERS" --anonymous-auth false \
  --service-account-signing-key-file "$W/sa.key" --service-account-key-file "$W/sa.pub" \
  --manifest-dir "$M" >"$W/apiserver.log" 2>&1 &
API_PID=$!
sleep 150
if kill -0 "$API_PID" 2>/dev/null; then pass "apiserver still waiting after 150 s"; else fail "apiserver exited while the datastore was down"; fi
code=$(get /readyz)
[ "$code" = 000 ] && pass "nothing served while the datastore is down" || fail "served $code while the datastore was down"
grep -q 'datastore has not answered for' "$W/apiserver.log" && pass "the wait is an ERROR in the log" \
  || fail "no 'datastore has not answered' line after 150 s"
t0=$(date +%s)
start_store
ready=
for _ in $(seq 360); do
  kill -0 "$API_PID" 2>/dev/null || { fail "apiserver exited"; break; }
  [ "$(get /readyz)" = 200 ] && { ready=1; break; }
  sleep 1
done
[ -n "$ready" ] && echo "NOTE  ready $(( $(date +%s) - t0 )) s after the datastore started" || fail "never ready after the datastore started"
grep -q 'manifests: 2 file(s) .* 4 created, 0 reconciled, 0 already present, 0 failed' "$W/apiserver.log" \
  && pass "manifest pass: 4 created, 0 failed" \
  || fail "manifest pass: $(grep 'manifests: 2 file' "$W/apiserver.log" | tail -1)"
for p in /apis/apiextensions.k8s.io/v1/customresourcedefinitions/widgets.rk293.example.io \
         /apis/rbac.authorization.k8s.io/v1/clusterroles/rk293-role \
         /api/v1/namespaces/default/configmaps/rk293 \
         /apis/rk293.example.io/v1/widgets/w1; do
  code=$(get "$p")
  [ "$code" = 200 ] && pass "applied: $p" || fail "$p: $code $(head -c 200 "$W/out")"
done

# --- 2. datastore gone under a serving apiserver: 503, not 403 ----------------
NOBODY=$(token rk293-nobody '[]')
code=$(get /api/v1/namespaces/default/configmaps "$NOBODY")
[ "$code" = 403 ] && pass "no grant, datastore up: 403" || fail "no grant, datastore up: $code"
kill "$STORE_PID"; wait "$STORE_PID" 2>/dev/null
code=$(get /api/v1/namespaces/default/configmaps "$NOBODY")
[ "$code" = 503 ] && pass "no grant, datastore down: 503" || fail "no grant, datastore down: $code $(head -c 200 "$W/out")"
grep -qi '^retry-after: 1' "$W/hdr" && pass "503 carries Retry-After" || fail "no Retry-After: $(tr -d '\r' <"$W/hdr" | tr '\n' ' ')"
start_store
ok=
for _ in $(seq 60); do
  [ "$(get /api/v1/namespaces/default/configmaps "$NOBODY")" = 403 ] && { ok=1; break; }
  sleep 1
done
[ -n "$ok" ] && pass "datastore back: 403 again" || fail "datastore back: still $(get /api/v1/namespaces/default/configmaps "$NOBODY")"

report
