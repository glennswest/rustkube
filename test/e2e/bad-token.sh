#!/usr/bin/env bash
#
# A presented token nothing accepts is 401, even with anonymous auth on
# (#115): a real apiserver on fastetcd with --anonymous-auth true. Anonymous
# is only for requests with no credentials. Upstream's behaviour; `sc login`
# tells a bad token from a good one without permission by it.
#
# - no Authorization header: anonymous — discovery 200, namespaces 403
# - a garbage token, a JWT signed by another key, an empty "Bearer ": the
#   first two 401 with a Status (reason Unauthorized), the empty one anonymous
# - the admin token: 200
#
# Exit status is the number of failed checks.
RK_ANONYMOUS_AUTH=true
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

code() { # <authorization header or ""> <path> — HTTP code, body in $W/out
  if [ -n "$1" ]; then
    curl -sk -o "$W/out" -w '%{http_code}' -H "Authorization: $1" "$API$2"
  else
    curl -sk -o "$W/out" -w '%{http_code}' "$API$2"
  fi
}
expect() { # <what> <want> <header> <path>
  local got; got=$(code "$3" "$4")
  [ "$got" = "$2" ] && pass "$1 ($got)" || fail "$1: want $2, got $got $(head -c 200 "$W/out")"
}
status_reason() { python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); print(s.get("kind"), s.get("reason"), s.get("code"))' "$W/out" 2>/dev/null; }

expect "no credentials: anonymous reads discovery" 200 "" /api
expect "no credentials: anonymous may not list namespaces" 403 "" /api/v1/namespaces
expect "garbage token: refused, not anonymous" 401 "Bearer not-a-token" /api
[ "$(status_reason)" = "Status Unauthorized 401" ] && pass "the 401 is a Status, reason Unauthorized" \
  || fail "401 body: $(head -c 200 "$W/out")"
expect "garbage token on a resource: 401, not anonymous's 403" 401 "Bearer not-a-token" /api/v1/namespaces
OTHER=$(python3 - <<'PY'
import base64, json
b = lambda d: base64.urlsafe_b64encode(json.dumps(d).encode()).rstrip(b"=").decode()
print(b({"alg": "RS256", "typ": "JWT"}) + "." + b({"sub": "system:serviceaccount:default:x", "exp": 4102444800}) + ".c2lnbmF0dXJl")
PY
)
expect "a JWT not signed by this cluster: 401" 401 "Bearer $OTHER" /api
expect "lower-case scheme, garbage token: 401" 401 "bearer not-a-token" /api
expect "an empty Bearer token is no credentials: anonymous" 200 "Bearer " /api
expect "the admin token: 200" 200 "Bearer $ADMIN" /api/v1/namespaces
report
