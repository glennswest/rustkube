#!/usr/bin/env bash
#
# Watch DELETED events (#100), against a real apiserver on a real fastetcd:
# for a namespaced custom resource, a cluster-scoped one and a built-in, the
# DELETED event must name the object where it lived (so an informer can drop
# it) and carry its last state; a label-selected watch hears only about what
# it selected.
#
#   test/e2e/watch-deleted.sh       # from the checkout; builds what it runs
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

req() { # <method> <path> [json] — prints the HTTP code
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H 'Content-Type: application/json' "$API$2" ${3:+-d "$3"}
}
watch_to() { # <file> <path> — a watch stream in the background
  curl -sNk -H "Authorization: Bearer $ADMIN" "$API$2" >"$W/$1" &
}
crd() { # <plural> <kind> <scope>
  req POST /apis/apiextensions.k8s.io/v1/customresourcedefinitions "{\"apiVersion\":\"apiextensions.k8s.io/v1\",\"kind\":\"CustomResourceDefinition\",\"metadata\":{\"name\":\"$1.demo.io\"},\"spec\":{\"group\":\"demo.io\",\"scope\":\"$3\",\"names\":{\"plural\":\"$1\",\"singular\":\"${2,,}\",\"kind\":\"$2\"},\"versions\":[{\"name\":\"v1\",\"served\":true,\"storage\":true,\"schema\":{\"openAPIV3Schema\":{\"type\":\"object\",\"x-kubernetes-preserve-unknown-fields\":true}}}]}}" >/dev/null
}
# <what> <file> <namespace or ""> <name> <a string of the last state>
expect_deleted() {
  local what=$1 f=$2 ns=$3 name=$4 last=$5 line
  line=$(grep '"type":"DELETED"' "$W/$f" | grep "\"name\":\"$name\"" | head -1)
  [ -n "$line" ] || { fail "$what: no DELETED event: $(cat "$W/$f")"; return; }
  echo "      $line"
  if [ -n "$ns" ]; then
    grep -q "\"namespace\":\"$ns\"" <<<"$line" && pass "$what: DELETED names namespace $ns" \
      || fail "$what: wrong namespace"
  else
    grep -q '"namespace"' <<<"$line" && fail "$what: a cluster-scoped object was given a namespace" \
      || pass "$what: DELETED has no namespace"
  fi
  grep -q -- "$last" <<<"$line" && pass "$what: DELETED carries the last state" \
    || fail "$what: DELETED lacks $last"
}

crd widgets Widget Namespaced
crd gadgets Gadget Cluster
for _ in $(seq 20); do
  [ "$(req GET /apis/demo.io/v1/namespaces/default/widgets)" = 200 ] \
    && [ "$(req GET /apis/demo.io/v1/gadgets)" = 200 ] && break
  sleep 1
done
req POST /apis/demo.io/v1/namespaces/default/widgets '{"apiVersion":"demo.io/v1","kind":"Widget","metadata":{"name":"w1","labels":{"app":"web"}},"spec":{"size":7}}' >/dev/null
req POST /apis/demo.io/v1/gadgets '{"apiVersion":"demo.io/v1","kind":"Gadget","metadata":{"name":"g1"},"spec":{"size":8}}' >/dev/null
req POST /api/v1/namespaces/default/configmaps '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"cm1"},"data":{"k":"v9"}}' >/dev/null

watch_to widgets.json "/apis/demo.io/v1/namespaces/default/widgets?watch=true"
watch_to widgets-other.json "/apis/demo.io/v1/namespaces/default/widgets?watch=true&labelSelector=app%3Dother"
watch_to gadgets.json "/apis/demo.io/v1/gadgets?watch=true"
watch_to cms.json "/api/v1/namespaces/default/configmaps?watch=true"
sleep 3

req DELETE /apis/demo.io/v1/namespaces/default/widgets/w1 >/dev/null
req DELETE /apis/demo.io/v1/gadgets/g1 >/dev/null
req DELETE /api/v1/namespaces/default/configmaps/cm1 >/dev/null
sleep 3

expect_deleted "namespaced custom resource" widgets.json default w1 '"size":7'
expect_deleted "cluster-scoped custom resource" gadgets.json "" g1 '"size":8'
expect_deleted "configmap" cms.json default cm1 '"v9"'
grep -q '"type":"DELETED"' "$W/widgets-other.json" \
  && fail "a watch for app=other heard w1 (app=web) deleted" \
  || pass "a label-selected watch hears only its own deletions"

report
