#!/usr/bin/env bash
#
# In-place pod resize, the `pods/resize` subresource (#136), against a real
# apiserver on fastetcd (no kubelet: what the apiserver accepts and stores).
#
# - discovery lists pods/resize (get, patch, update)
# - kubectl patch --subresource=resize changes cpu requests/limits and
#   resizePolicy; an image change in the same patch is ignored
# - PUT /resize of the Pod with its resourceVersion; a stale one is 409
# - refused 422: QoS class change, a removed request, a non-cpu/memory
#   resource, request above limit, a non-sidecar init container
# - a Pod whose running container reports no status `resources` (a kubelet
#   that does not resize) is refused; once it reports them, resized
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
P=/api/v1/namespaces/default/pods

api GET /api/v1 | grep -q '"pods/resize"' && pass "discovery lists pods/resize" || fail "discovery lists pods/resize"

api POST $P '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"rz"},"spec":{
  "initContainers":[{"name":"init","image":"i","resources":{"requests":{"cpu":"10m"},"limits":{"cpu":"10m"}}}],
  "containers":[{"name":"c","image":"pause","resources":{"requests":{"cpu":"100m","memory":"64Mi"},"limits":{"cpu":"200m","memory":"64Mi"}},
  "resizePolicy":[{"resourceName":"cpu","restartPolicy":"NotRequired"}]}]}}' >/dev/null
[ "$(cat "$W/code")" = 201 ] && pass "pod created" || fail "pod created ($(cat "$W/code"))"

k patch pod rz --subresource=resize --type=strategic -p \
  '{"spec":{"containers":[{"name":"c","image":"other","resources":{"requests":{"cpu":"150m"},"limits":{"cpu":"300m"}},"resizePolicy":[{"resourceName":"cpu","restartPolicy":"RestartContainer"}]}]}}' >/dev/null 2>"$W/err" \
  && pass "kubectl patch --subresource=resize" || fail "kubectl patch --subresource=resize ($(cat "$W/err"))"
got=$(api GET $P/rz | jq_ 'd["spec"]["containers"][0]["resources"]["requests"]["cpu"]+" "+d["spec"]["containers"][0]["resources"]["limits"]["cpu"]+" "+d["spec"]["containers"][0]["resizePolicy"][0]["restartPolicy"]+" "+d["spec"]["containers"][0]["image"]')
[ "$got" = "150m 300m RestartContainer pause" ] && pass "resources + resizePolicy changed, image kept ($got)" || fail "resize result ($got)"

pod=$(api GET $P/rz/resize)
rv=$(echo "$pod" | jq_ 'd["metadata"]["resourceVersion"]')
body=$(echo "$pod" | python3 -c 'import json,sys; p=json.load(sys.stdin); p["spec"]["containers"][0]["resources"]["requests"]["cpu"]="120m"; print(json.dumps(p))')
api PUT $P/rz/resize "$body" >/dev/null
[ "$(cat "$W/code")" = 200 ] && [ "$(api GET $P/rz | jq_ 'd["spec"]["containers"][0]["resources"]["requests"]["cpu"]')" = 120m ] \
  && pass "PUT /resize with its resourceVersion" || fail "PUT /resize ($(cat "$W/code"))"
api PUT $P/rz/resize "$body" >/dev/null
[ "$(cat "$W/code")" = 409 ] && pass "PUT /resize with a stale resourceVersion: 409" || fail "stale PUT ($(cat "$W/code"))"

refused() { # <what> <strategic patch> <message fragment>
  out=$(api PATCH $P/rz/resize "$2" application/strategic-merge-patch+json)
  if [ "$(cat "$W/code")" = 422 ] && echo "$out" | grep -qF "$3"; then pass "$1: 422"; else fail "$1 ($(cat "$W/code") $out)"; fi
}
refused "non-sidecar init container" '{"spec":{"containers":[{"name":"c","resources":{"requests":{"cpu":"300m","memory":"64Mi"},"limits":{"cpu":"300m","memory":"64Mi"}}}],"initContainers":[{"name":"init","resources":{"requests":{"cpu":"10m","memory":"1Mi"},"limits":{"cpu":"10m","memory":"1Mi"}}}]}}' "resources for non-sidecar init containers are immutable"
refused "request above limit" '{"spec":{"containers":[{"name":"c","resources":{"requests":{"cpu":"500m"}}}]}}' "must be less than or equal to cpu limit of 300m"
refused "a removed request" '{"spec":{"containers":[{"name":"c","resources":{"requests":{"memory":null}}}]}}' "resource requests cannot be removed"
refused "ephemeral-storage" '{"spec":{"containers":[{"name":"c","resources":{"limits":{"ephemeral-storage":"1Gi"}}}]}}' "only cpu and memory resources are mutable"

# A Pod made Guaranteed-able: a lone container with cpu == memory.
api POST $P '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"rz2"},"spec":{"containers":[{"name":"c","image":"pause",
  "resources":{"requests":{"cpu":"100m","memory":"64Mi"},"limits":{"cpu":"200m","memory":"64Mi"}}}]}}' >/dev/null
out=$(api PATCH $P/rz2/resize '{"spec":{"containers":[{"name":"c","resources":{"requests":{"cpu":"200m"}}}]}}' application/strategic-merge-patch+json)
[ "$(cat "$W/code")" = 422 ] && echo "$out" | grep -qF "Pod QOS Class may not change" && pass "Burstable → Guaranteed: 422" || fail "QoS ($(cat "$W/code") $out)"

status() { # <containerStatus json>
  s=$(api GET $P/rz2 | python3 -c "import json,sys; p=json.load(sys.stdin); p['status']['containerStatuses']=[json.loads(sys.argv[1])]; print(json.dumps(p))" "$1")
  api PUT $P/rz2/status "$s" >/dev/null
}
status '{"name":"c","image":"pause","imageID":"","ready":true,"restartCount":0,"state":{"running":{"startedAt":"2026-10-07T00:00:00Z"}}}'
out=$(api PATCH $P/rz2/resize '{"spec":{"containers":[{"name":"c","resources":{"limits":{"cpu":"250m"}}}]}}' application/strategic-merge-patch+json)
[ "$(cat "$W/code")" = 422 ] && echo "$out" | grep -qF "Pod running on node without support for resize" && pass "running, no status resources: refused" || fail "unsupported node ($(cat "$W/code") $out)"
status '{"name":"c","image":"pause","imageID":"","ready":true,"restartCount":0,"state":{"running":{"startedAt":"2026-10-07T00:00:00Z"}},"resources":{"requests":{"cpu":"100m","memory":"64Mi"},"limits":{"cpu":"200m","memory":"64Mi"}}}'
api PATCH $P/rz2/resize '{"spec":{"containers":[{"name":"c","resources":{"limits":{"cpu":"250m"}}}]}}' application/strategic-merge-patch+json >/dev/null
[ "$(cat "$W/code")" = 200 ] && pass "running with status resources: resized" || fail "supported node ($(cat "$W/code"))"
report
