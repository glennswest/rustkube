#!/usr/bin/env bash
#
# A kubelet's events (#282): a node identity (system:node:<n>, group
# system:nodes) records events in any namespace; in a namespace being
# deleted the 403 carries upstream's `NamespaceTerminating` cause, which
# client-go's event recorder drops quietly instead of logging a rejection.
#
# - node events in a fresh namespace and in kube-system: 201
# - a namespace held Terminating (no controller-manager to finish it): 403
#   Forbidden, upstream's message, cause NamespaceTerminating on
#   metadata.namespace
# - a namespace that does not exist: 404 NotFound
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

NODE=$(token system:node:n1 '["system:nodes"]')
post() { # <token> <path> <json> — HTTP code, body in $W/out
  curl -sk -o "$W/out" -w '%{http_code}' -X POST -H "Authorization: Bearer $1" \
    -H 'Content-Type: application/json' "$API$2" -d "$3"
}
event() { # <namespace> <name>
  printf '{"apiVersion":"v1","kind":"Event","metadata":{"name":"%s","namespace":"%s"},"involvedObject":{"kind":"Pod","namespace":"%s","name":"p"},"reason":"NetworkNotReady","message":"rig","type":"Warning","source":{"component":"kubelet","host":"n1"},"count":1}' "$2" "$1" "$1"
}
ns() { post "$ADMIN" /api/v1/namespaces "{\"apiVersion\":\"v1\",\"kind\":\"Namespace\",\"metadata\":{\"name\":\"$1\"}}" >/dev/null; }

ns events-live
got=$(post "$NODE" /api/v1/namespaces/events-live/events "$(event events-live e1)")
[ "$got" = 201 ] && pass "node event in a new namespace: 201" || fail "node event in a new namespace: $got $(head -c 300 "$W/out")"
got=$(post "$NODE" /api/v1/namespaces/kube-system/events "$(event kube-system e2)")
[ "$got" = 201 ] && pass "node event in kube-system: 201" || fail "node event in kube-system: $got $(head -c 300 "$W/out")"

ns events-going
curl -sk -o /dev/null -X DELETE -H "Authorization: Bearer $ADMIN" "$API/api/v1/namespaces/events-going"
phase=$(curl -sk -H "Authorization: Bearer $ADMIN" "$API/api/v1/namespaces/events-going" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",{}).get("phase"))')
[ "$phase" = Terminating ] && pass "deleted namespace held Terminating" || fail "namespace phase after DELETE: $phase"
got=$(post "$NODE" /api/v1/namespaces/events-going/events "$(event events-going e3)")
[ "$got" = 403 ] && pass "node event in a terminating namespace: 403" || fail "terminating namespace: want 403, got $got $(head -c 300 "$W/out")"
python3 - "$W/out" <<'PY' && pass "403 is upstream's: reason Forbidden, cause NamespaceTerminating" || fail "403 body: $(head -c 400 "$W/out")"
import json, sys
s = json.load(open(sys.argv[1]))
assert s["reason"] == "Forbidden", s
assert "unable to create new content in namespace events-going because it is being terminated" in s["message"], s
assert {"reason": "NamespaceTerminating", "message": "namespace events-going is being terminated",
        "field": "metadata.namespace"} in s["details"]["causes"], s
PY

got=$(post "$NODE" /api/v1/namespaces/events-never/events "$(event events-never e4)")
[ "$got" = 404 ] && pass "node event in a missing namespace: 404" || fail "missing namespace: want 404, got $got $(head -c 300 "$W/out")"
report
