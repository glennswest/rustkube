#!/usr/bin/env bash
#
# Ordered namespace deletion (#285, upstream's OrderedNamespaceDeletion, GA
# in 1.34), with the controller-manager running: the Pods of a terminating
# namespace are deleted first, and nothing else is deleted while one is left.
#
# - a Pod held by a finalizer, a ConfigMap and a Secret; DELETE the namespace
# - 8 s later: the Pod is deleting (deletionTimestamp), the ConfigMap and the
#   Secret are untouched (no deletionTimestamp, still there)
# - the Pod's finalizer removed: the ConfigMap, the Secret and then the
#   namespace go
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

start_controller_manager

k() { curl -sk -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' "$@"; }
code() { curl -sk -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $ADMIN" "$API$1"; }
deleting() { k "$API$1" | python3 -c 'import json,sys; print("yes" if json.load(sys.stdin).get("metadata",{}).get("deletionTimestamp") else "no")'; }

NS=order
k -X POST "$API/api/v1/namespaces" -d "{\"apiVersion\":\"v1\",\"kind\":\"Namespace\",\"metadata\":{\"name\":\"$NS\"}}" >/dev/null
k -X POST "$API/api/v1/namespaces/$NS/pods" -d '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"held","finalizers":["example.com/hold"]},"spec":{"containers":[{"name":"c","image":"pause"}]}}' >/dev/null
k -X POST "$API/api/v1/namespaces/$NS/configmaps" -d '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"cfg"},"data":{"a":"1"}}' >/dev/null
k -X POST "$API/api/v1/namespaces/$NS/secrets" -d '{"apiVersion":"v1","kind":"Secret","metadata":{"name":"sec"},"stringData":{"a":"1"}}' >/dev/null
[ "$(code /api/v1/namespaces/$NS/configmaps/cfg)" = 200 ] && pass "namespace with a held Pod, a ConfigMap, a Secret" || fail "setup"

k -X DELETE "$API/api/v1/namespaces/$NS" >/dev/null
sleep 8
[ "$(deleting /api/v1/namespaces/$NS/pods/held)" = yes ] && pass "the Pod is being deleted first" || fail "the Pod was not deleted"
for o in configmaps/cfg secrets/sec; do
  c=$(code /api/v1/namespaces/$NS/$o)
  d=$(deleting /api/v1/namespaces/$NS/$o)
  [ "$c" = 200 ] && [ "$d" = no ] && pass "$o untouched while the Pod is left" || fail "$o: HTTP $c, deleting $d, while a Pod was left"
done

k -X PATCH -H 'Content-Type: application/merge-patch+json' "$API/api/v1/namespaces/$NS/pods/held" -d '{"metadata":{"finalizers":null}}' >/dev/null
for _ in $(seq 30); do
  [ "$(code /api/v1/namespaces/$NS)" = 404 ] && break
  sleep 1
done
for o in pods/held configmaps/cfg secrets/sec; do
  c=$(code /api/v1/namespaces/$NS/$o)
  [ "$c" = 404 ] && pass "$o gone after the Pod" || fail "$o still there: HTTP $c"
done
[ "$(code /api/v1/namespaces/$NS)" = 404 ] && pass "namespace gone" || fail "namespace still there"
report
