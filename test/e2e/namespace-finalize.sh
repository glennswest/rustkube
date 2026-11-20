#!/usr/bin/env bash
#
# Namespace /finalize preconditions (#366, review finding f33be8cc42cb),
# against a real apiserver on a real fastetcd, no controller-manager (the
# rig plays the namespace controller and sends what it sends: the uid and
# resourceVersion it observed):
#
# - a terminating namespace: a finalize naming another uid is a 409, and so
#   is one naming an older resourceVersion; the namespace stays, its
#   `kubernetes` finalizer kept
# - deleted and made again under the same name, terminating: the request
#   observed for the first one is a 409, and the replacement stays
# - the current uid and resourceVersion: finalized, the namespace goes
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

NS=fin
req() { # <method> <path> [json] — HTTP code; body in $W/out
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H 'Content-Type: application/json' "$API$2" ${3:+-d "$3"}
}
field() { python3 -c "import json; print(json.load(open('$W/out'))$1)"; }
observe() { req GET /api/v1/namespaces/$NS >/dev/null; UID_=$(field '["metadata"]["uid"]'); RV=$(field '["metadata"]["resourceVersion"]'); }
finalize() { # <uid> <rv> — HTTP code
  req PUT /api/v1/namespaces/$NS/finalize \
    "{\"apiVersion\":\"v1\",\"kind\":\"Namespace\",\"metadata\":{\"name\":\"$NS\",\"uid\":\"$1\",\"resourceVersion\":\"$2\"},\"spec\":{\"finalizers\":[]}}"
}
held() { # still there, with its kubernetes finalizer
  [ "$(req GET /api/v1/namespaces/$NS)" = 200 ] && [ "$(field '["spec"]["finalizers"]')" = "['kubernetes']" ]
}
make() {
  req POST /api/v1/namespaces "{\"apiVersion\":\"v1\",\"kind\":\"Namespace\",\"metadata\":{\"name\":\"$NS\"}}" >/dev/null
  req DELETE /api/v1/namespaces/$NS >/dev/null
}

make; observe
[ -n "$UID_" ] && held && pass "terminating namespace, kubernetes finalizer" || fail "setup: $(cat "$W/out")"
FIRST_UID=$UID_ FIRST_RV=$RV

c=$(finalize not-the-uid "$RV")
[ "$c" = 409 ] && grep -q 'UID in precondition' "$W/out" && pass "another uid: 409" || fail "wrong uid: $c $(cat "$W/out")"
held && pass "kept after the wrong-uid request" || fail "wrong uid changed it: $(cat "$W/out")"

c=$(curl -sk -o "$W/out" -w '%{http_code}' -X PATCH -H "Authorization: Bearer $ADMIN" \
  -H 'Content-Type: application/merge-patch+json' "$API/api/v1/namespaces/$NS" -d '{"metadata":{"labels":{"touched":"yes"}}}')
[ "$c" = 200 ] || fail "setup: label patch $c $(cat "$W/out")"
c=$(finalize "$FIRST_UID" "$FIRST_RV")
[ "$c" = 409 ] && grep -q 'the object has been modified' "$W/out" && pass "an older resourceVersion: 409" || fail "stale rv: $c $(cat "$W/out")"
held && pass "kept after the stale request" || fail "stale request changed it"

# Finalized for real, then made again under the same name.
observe
c=$(finalize "$UID_" "$RV")
[ "$c" = 200 ] && [ "$(req GET /api/v1/namespaces/$NS)" = 404 ] && pass "current uid and version: finalized, gone" \
  || fail "current finalize: $c, GET $(req GET /api/v1/namespaces/$NS)"
make; observe
[ "$UID_" != "$FIRST_UID" ] && held && pass "made again under the same name, terminating" || fail "recreate: $UID_"
c=$(finalize "$FIRST_UID" "$FIRST_RV")
[ "$c" = 409 ] && pass "the first namespace's request against its replacement: 409" || fail "late request: $c $(cat "$W/out")"
held && pass "the replacement stays, finalizer kept" || fail "the late request finalized the replacement"

report
