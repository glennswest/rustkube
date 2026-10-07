#!/usr/bin/env bash
#
# The scale subresource (#86): `kubectl scale` against a real apiserver on
# fastetcd (no controllers: it is the object's spec.replicas that changes).
#
# - kubectl scale deployment, replicaset and statefulset → spec.replicas
# - kubectl scale --current-replicas with the wrong count is refused
# - GET …/scale is an autoscaling/v1 Scale: spec/status replicas, selector,
#   the object's resourceVersion; PUT with a stale resourceVersion → 409,
#   replicas -1 → 422; a merge PATCH of the Scale changes the replicas
# - a CRD declaring subresources.scale: kubectl scale works through its
#   paths; a CRD without one → 404
# - discovery lists deployments/replicasets/statefulsets/scale and the CR's
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

KUBECTL=${RK_TOOLS:+$RK_TOOLS/kubectl}
[ -x "${KUBECTL:-}" ] || KUBECTL=$(command -v kubectl || true)
if [ -z "$KUBECTL" ]; then
  KUBECTL=$W/kubectl
  curl -sfL -o "$KUBECTL" "https://dl.k8s.io/$(curl -sfL "https://dl.k8s.io/release/$KUBECTL_CHANNEL.txt")/bin/linux/amd64/kubectl" || exit 100
  chmod +x "$KUBECTL"
fi
cat >"$W/admin.kubeconfig" <<KC
apiVersion: v1
kind: Config
clusters: [ { name: rk, cluster: { server: "$API", insecure-skip-tls-verify: true } } ]
users: [ { name: u, user: { token: "$ADMIN" } } ]
contexts: [ { name: rk, context: { cluster: rk, user: u, namespace: default } } ]
current-context: rk
KC
k() { "$KUBECTL" --kubeconfig "$W/admin.kubeconfig" "$@"; }
api() { # <method> <path> [json] [content-type] — body to stdout, code in $W/code
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H "Content-Type: ${4:-application/json}" "$API$2" ${3:+-d "$3"} >"$W/code"; cat "$W/out"
}
jq_() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1" 2>/dev/null; }
replicas() { k get "$1" -o jsonpath='{.spec.replicas}'; }

tmpl='{"metadata":{"labels":{"app":"w"}},"spec":{"containers":[{"name":"c","image":"unused"}]}}'
api POST /apis/apps/v1/namespaces/default/deployments "{\"apiVersion\":\"apps/v1\",\"kind\":\"Deployment\",\"metadata\":{\"name\":\"web\"},\"spec\":{\"replicas\":1,\"selector\":{\"matchLabels\":{\"app\":\"w\"}},\"template\":$tmpl}}" >/dev/null
api POST /apis/apps/v1/namespaces/default/replicasets "{\"apiVersion\":\"apps/v1\",\"kind\":\"ReplicaSet\",\"metadata\":{\"name\":\"rs\"},\"spec\":{\"replicas\":1,\"selector\":{\"matchLabels\":{\"app\":\"w\"}},\"template\":$tmpl}}" >/dev/null
api POST /apis/apps/v1/namespaces/default/statefulsets "{\"apiVersion\":\"apps/v1\",\"kind\":\"StatefulSet\",\"metadata\":{\"name\":\"db\"},\"spec\":{\"replicas\":1,\"serviceName\":\"db\",\"selector\":{\"matchLabels\":{\"app\":\"w\"}},\"template\":$tmpl}}" >/dev/null

for obj in deployment/web replicaset/rs statefulset/db; do
  out=$(k scale "$obj" --replicas=3 2>&1)
  [ "$(replicas "$obj")" = 3 ] && pass "kubectl scale $obj --replicas=3" || fail "kubectl scale $obj: $out"
done
out=$(k scale deployment/web --current-replicas=2 --replicas=5 2>&1)
[ "$(replicas deployment/web)" = 3 ] && pass "--current-replicas mismatch refused" || fail "--current-replicas: $out"

S=/apis/apps/v1/namespaces/default/deployments/web/scale
api GET $S >"$W/scale.json"
[ "$(jq_ 'd["kind"]=="Scale" and d["apiVersion"]=="autoscaling/v1" and d["spec"]["replicas"]==3 and d["status"]["selector"]=="app=w" and d["metadata"]["resourceVersion"]!=""' <"$W/scale.json")" = True ] \
  && pass "GET scale: an autoscaling/v1 Scale with replicas, selector and resourceVersion" || fail "GET scale: $(cat "$W/scale.json")"
api PUT $S '{"apiVersion":"autoscaling/v1","kind":"Scale","metadata":{"name":"web","resourceVersion":"1"},"spec":{"replicas":4}}' >/dev/null
[ "$(cat "$W/code")" = 409 ] && pass "PUT with a stale resourceVersion: 409" || fail "stale PUT: $(cat "$W/code")"
api PUT $S '{"apiVersion":"autoscaling/v1","kind":"Scale","metadata":{"name":"web"},"spec":{"replicas":-1}}' >/dev/null
[ "$(cat "$W/code")" = 422 ] && pass "replicas -1: 422" || fail "negative: $(cat "$W/code")"
api PATCH $S '{"spec":{"replicas":2}}' application/merge-patch+json >/dev/null
[ "$(cat "$W/code")" = 200 ] && [ "$(replicas deployment/web)" = 2 ] && pass "merge PATCH of the Scale" || fail "PATCH: $(cat "$W/code") $(cat "$W/out")"

# --- custom resources --------------------------------------------------------------
crd() { # <plural> <scale json or empty>
  local sub='"status":{}'; [ -n "$2" ] && sub="$sub,\"scale\":$2"
  api POST /apis/apiextensions.k8s.io/v1/customresourcedefinitions "{\"apiVersion\":\"apiextensions.k8s.io/v1\",\"kind\":\"CustomResourceDefinition\",
    \"metadata\":{\"name\":\"$1.scale.example.com\"},\"spec\":{\"group\":\"scale.example.com\",\"scope\":\"Namespaced\",
    \"names\":{\"plural\":\"$1\",\"singular\":\"${1%s}\",\"kind\":\"K$1\",\"listKind\":\"K$1List\"},
    \"versions\":[{\"name\":\"v1\",\"served\":true,\"storage\":true,\"subresources\":{$sub},
      \"schema\":{\"openAPIV3Schema\":{\"type\":\"object\",\"x-kubernetes-preserve-unknown-fields\":true}}}]}}" >/dev/null
}
crd clusters '{"specReplicasPath":".spec.size","statusReplicasPath":".status.size","labelSelectorPath":".status.selector"}'
crd plains ""
for _ in $(seq 40); do api GET /apis/scale.example.com/v1 >/dev/null; grep -q '"plains"' "$W/out" && grep -q '"clusters"' "$W/out" && break; sleep 0.5; done
grep -q '"clusters/scale"' "$W/out" && ! grep -q '"plains/scale"' "$W/out" \
  && pass "discovery: clusters/scale listed, plains/scale not" || fail "CR discovery: $(cat "$W/out")"
api POST /apis/scale.example.com/v1/namespaces/default/clusters '{"apiVersion":"scale.example.com/v1","kind":"Kclusters","metadata":{"name":"c1"},"spec":{"size":1}}' >/dev/null
api POST /apis/scale.example.com/v1/namespaces/default/plains '{"apiVersion":"scale.example.com/v1","kind":"Kplains","metadata":{"name":"p1"},"spec":{"size":1}}' >/dev/null
out=$(k scale clusters.scale.example.com/c1 --replicas=4 2>&1)
[ "$(k get clusters.scale.example.com/c1 -o jsonpath='{.spec.size}')" = 4 ] && pass "kubectl scale on a CR, through specReplicasPath" || fail "CR scale: $out"
api GET /apis/scale.example.com/v1/namespaces/default/plains/p1/scale >/dev/null
[ "$(cat "$W/code")" = 404 ] && pass "a CRD without subresources.scale: 404" || fail "plain CR scale: $(cat "$W/code")"

api GET /apis/apps/v1 >/dev/null
for r in deployments replicasets statefulsets; do
  grep -q "\"$r/scale\"" "$W/out" && pass "discovery lists $r/scale" || fail "no $r/scale in apps/v1 discovery"
done
report
