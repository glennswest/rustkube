#!/usr/bin/env bash
#
# Pod-bound ServiceAccount tokens (#182) against a real apiserver on a real
# fastetcd: TokenRequest honours audiences, expirationSeconds and
# boundObjectRef; the token authenticates to the apiserver only while its
# pod (and ServiceAccount) is the one it was issued for; TokenReview checks
# audiences and reports the pod and its node.
#
#   test/e2e/bound-token.sh          # from the checkout; builds what it runs
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

ISS=https://kubernetes.default.svc
NS=bt
req() { # <method> <path> [json] — HTTP code; body in $W/out
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H 'Content-Type: application/json' "$API$2" ${3:+-d "$3"}
}
as() { # <token> — HTTP code of GET /api as that token
  curl -sk -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $1" "$API/api"
}
jq_() { python3 -c "import json,sys; d=json.load(open('$W/out')); print($1)"; }
claims() { # <token> — the payload as JSON in $W/claims
  python3 - "$1" >"$W/claims" <<'PY'
import base64, json, sys
p = sys.argv[1].split('.')[1]
print(json.dumps(json.loads(base64.urlsafe_b64decode(p + '=' * (-len(p) % 4)))))
PY
}
cl() { python3 -c "import json; c=json.load(open('$W/claims')); print($1)"; }
tokreq() { # <spec-json> — HTTP code; token in $TOK
  local c
  c=$(req POST /api/v1/namespaces/$NS/serviceaccounts/app/token \
    "{\"apiVersion\":\"authentication.k8s.io/v1\",\"kind\":\"TokenRequest\",\"spec\":$1}")
  TOK=; [ "$c" = 201 ] || [ "$c" = 200 ] && TOK=$(jq_ 'd["status"]["token"]')
  echo "$c"
}
review() { # <token> <audiences-json> — status JSON in $W/out
  req POST /apis/authentication.k8s.io/v1/tokenreviews \
    "{\"apiVersion\":\"authentication.k8s.io/v1\",\"kind\":\"TokenReview\",\"spec\":{\"token\":\"$1\",\"audiences\":$2}}" >/dev/null
}
# <what> <token> <want code>: the apiserver's answer settles within 5 s
await() {
  local got
  for _ in $(seq 20); do
    got=$(as "$2"); [ "$got" = "$3" ] && { pass "$1 ($got)"; return; }
    sleep 0.25
  done
  fail "$1: want $3, got $got"
}

req POST /api/v1/namespaces "{\"apiVersion\":\"v1\",\"kind\":\"Namespace\",\"metadata\":{\"name\":\"$NS\"}}" >/dev/null
req POST /api/v1/namespaces/$NS/serviceaccounts '{"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"app"}}' >/dev/null
SA_UID=$(jq_ 'd["metadata"]["uid"]')
req POST /api/v1/nodes '{"apiVersion":"v1","kind":"Node","metadata":{"name":"node-a"}}' >/dev/null
NODE_UID=$(jq_ 'd["metadata"]["uid"]')
pod() { # <name> <sa>
  req POST /api/v1/namespaces/$NS/pods "{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{\"name\":\"$1\"},
    \"spec\":{\"serviceAccountName\":\"$2\",\"nodeName\":\"node-a\",\"containers\":[{\"name\":\"c\",\"image\":\"x\"}]}}" >/dev/null
  jq_ 'd["metadata"]["uid"]'
}
POD_UID=$(pod p app)
pod q default >/dev/null
[ -n "$SA_UID" ] && [ -n "$POD_UID" ] && [ -n "$NODE_UID" ] && pass "SA, node and pod created" \
  || fail "setup: sa=$SA_UID pod=$POD_UID node=$NODE_UID"

# --- the projected volume's request: pod-bound, 3607 s ------------------------
c=$(tokreq "{\"expirationSeconds\":3607,\"boundObjectRef\":{\"kind\":\"Pod\",\"apiVersion\":\"v1\",\"name\":\"p\",\"uid\":\"$POD_UID\"}}")
[ -n "$TOK" ] && pass "TokenRequest bound to the pod ($c)" || fail "TokenRequest: $c $(cat "$W/out")"
EXP_TS=$(jq_ 'd["status"]["expirationTimestamp"]')
POD_TOK=$TOK
claims "$POD_TOK"
[ "$(cl 'c["iss"]')" = "$ISS" ] && [ "$(cl 'c["aud"]')" = "['$ISS']" ] \
  && [ "$(cl 'c["sub"]')" = "system:serviceaccount:$NS:app" ] \
  && pass "iss, aud, sub" || fail "claims: $(cat "$W/claims")"
[ "$(cl 'c["kubernetes.io"]["pod"]')" = "{'name': 'p', 'uid': '$POD_UID'}" ] \
  && [ "$(cl 'c["kubernetes.io"]["serviceaccount"]["uid"]')" = "$SA_UID" ] \
  && [ "$(cl 'c["kubernetes.io"]["node"]["uid"]')" = "$NODE_UID" ] \
  && pass "kubernetes.io names the pod, its ServiceAccount and node" || fail "kubernetes.io: $(cat "$W/claims")"
[ "$(cl 'c["exp"]-c["iat"]')" = 31536000 ] && [ "$(cl 'c["kubernetes.io"]["warnafter"]-c["iat"]')" = 3607 ] \
  && pass "3607 s pod-bound: a year, warnafter 3607 s" || fail "extension: $(cat "$W/claims")"
IAT=$(cl 'c["iat"]')
[ $(( $(date -d "$EXP_TS" +%s) - IAT )) = 3607 ] && pass "response expires at 3607 s ($EXP_TS)" \
  || fail "expirationTimestamp $EXP_TS, iat $IAT"
[ "$(as "$POD_TOK")" = 200 ] && pass "pod token authenticates to the apiserver" || fail "pod token: $(as "$POD_TOK")"

review "$POD_TOK" '[]'
if [ "$(jq_ 'd["status"]["authenticated"]')" = True ] \
  && [ "$(jq_ 'd["status"]["user"]["extra"]["authentication.kubernetes.io/pod-name"]')" = "['p']" ] \
  && [ "$(jq_ 'd["status"]["user"]["extra"]["authentication.kubernetes.io/pod-uid"]')" = "['$POD_UID']" ] \
  && [ "$(jq_ 'd["status"]["user"]["extra"]["authentication.kubernetes.io/node-name"]')" = "['node-a']" ] \
  && [ "$(jq_ 'd["status"]["user"]["uid"]')" = "$SA_UID" ] \
  && [ "$(jq_ 'd["status"]["audiences"]')" = "['$ISS']" ]; then
  pass "TokenReview reports the pod, its node and the SA uid"
else fail "TokenReview: $(cat "$W/out")"; fi
review "$POD_TOK" '["vault"]'
[ "$(jq_ 'd["status"]["authenticated"]')" = False ] && pass "TokenReview for another audience: refused" \
  || fail "review vault: $(cat "$W/out")"

# --- audiences and expirationSeconds -----------------------------------------
tokreq '{"audiences":["vault"],"expirationSeconds":600}' >/dev/null
claims "$TOK"
[ "$(cl 'c["aud"]')" = "['vault']" ] && [ "$(cl 'c["exp"]-c["iat"]')" = 600 ] \
  && pass "audiences + expirationSeconds 600 honoured" || fail "vault token: $(cat "$W/claims")"
[ "$(as "$TOK")" = 401 ] && pass "a vault token does not authenticate to the apiserver" || fail "vault token: $(as "$TOK")"
review "$TOK" '["vault"]'
[ "$(jq_ 'd["status"]["authenticated"]')" = True ] && pass "TokenReview for vault: authenticated" \
  || fail "review vault token: $(cat "$W/out")"
c=$(tokreq '{"expirationSeconds":599}'); [ "$c" = 422 ] && pass "599 s refused (422)" || fail "599 s: $c"
c=$(tokreq '{"boundObjectRef":{"kind":"Pod","name":"p","uid":"not-the-uid"}}')
[ "$c" = 409 ] && pass "wrong pod uid refused (409)" || fail "wrong uid: $c"
c=$(tokreq '{"boundObjectRef":{"kind":"Pod","name":"q"}}')
[ "$c" = 400 ] && pass "pod of another ServiceAccount refused (400)" || fail "other SA pod: $c"
c=$(tokreq '{"boundObjectRef":{"kind":"Pod","name":"nope"}}')
[ "$c" = 404 ] && pass "missing pod refused (404)" || fail "missing pod: $c"
tokreq '{}' >/dev/null; UNBOUND=$TOK
[ "$(as "$UNBOUND")" = 200 ] && pass "unbound token authenticates" || fail "unbound: $(as "$UNBOUND")"
[ "$(as "$ADMIN")" = 200 ] && pass "a token with no aud/iss (stormcert's shape) still works" || fail "legacy: $(as "$ADMIN")"

# --- the pod goes: its token goes with it ------------------------------------
c=$(req DELETE "/api/v1/namespaces/$NS/pods/p" '{"kind":"DeleteOptions","apiVersion":"v1","gracePeriodSeconds":0}')
case "$c" in 200|202) pass "pod deleted ($c)" ;; *) fail "pod delete: $c $(cat "$W/out")" ;; esac
for _ in $(seq 20); do [ "$(req GET /api/v1/namespaces/$NS/pods/p)" = 404 ] && break; sleep 0.25; done
await "deleted pod: token refused" "$POD_TOK" 401
review "$POD_TOK" '[]'
[ "$(jq_ 'd["status"]["authenticated"]')" = False ] && pass "TokenReview: deleted pod's token not authenticated" \
  || fail "review after delete: $(cat "$W/out")"
NEW_UID=$(pod p app)
[ -n "$NEW_UID" ] && [ "$NEW_UID" != "$POD_UID" ] && pass "pod recreated under the same name" || fail "recreate: $NEW_UID"
sleep 1
[ "$(as "$POD_TOK")" = 401 ] && pass "recreated pod: old token still refused" || fail "after recreate: $(as "$POD_TOK")"

# --- the ServiceAccount goes: every token of it goes -------------------------
req DELETE /api/v1/namespaces/$NS/serviceaccounts/app >/dev/null
req POST /api/v1/namespaces/$NS/serviceaccounts '{"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"app"}}' >/dev/null
await "ServiceAccount recreated: unbound token refused" "$UNBOUND" 401

report
