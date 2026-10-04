#!/usr/bin/env bash
#
# --token-auth-file (#188) against a real apiserver on a real fastetcd: the
# line stormpump#78 writes from install-config's apiToken authenticates as
# system:admin in system:masters, and the file is followed — written after
# boot, rewritten, malformed, removed.
#
#   test/e2e/token-auth.sh           # from the checkout; builds what it runs
#
# Exit status is the number of failed checks.
mkdir -p "$PWD/tmp"
TD=$(mktemp -d "$PWD/tmp/token-auth.XXXXXX")
TOKFILE=$TD/token-auth.csv
RK_APISERVER_ARGS="--token-auth-file $TOKFILE"
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
trap 'cleanup; rm -rf "$TD"' EXIT

T1=$(openssl rand -hex 24)   # 48 hex characters, as storminstall generates
T2=$(openssl rand -hex 24)
line() { printf '%s,system:admin,system:admin,"system:masters"\n' "$1"; }
code() { # <token> [method path json] — HTTP code
  curl -sk -o "$W/out" -w '%{http_code}' -X "${2:-GET}" -H "Authorization: Bearer $1" \
    -H 'Content-Type: application/json' "$API${3:-/api/v1/namespaces}" ${4:+-d "$4"}
}
# <what> <token> <want code>: poll up to 20 s (the file is re-read every 5 s)
await() {
  local got
  for _ in $(seq 40); do
    got=$(code "$2"); [ "$got" = "$3" ] && { pass "$1 ($got)"; return; }
    sleep 0.5
  done
  fail "$1: want $3, got $got"
}

c=$(code "$T1"); [ "$c" = 401 ] && pass "no file yet: token refused (401)" || fail "no file yet: $c"
grep -q 'token auth file does not exist yet' "$W/apiserver.log" \
  && pass "missing file logged, apiserver up" || fail "missing file not logged"

# First boot writes the file after the apiserver is up.
(umask 077; line "$T1" >"$TOKFILE")
await "file written after boot: token accepted" "$T1" 200

c=$(code "$T1" POST /api/v1/namespaces '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"made-by-apitoken"}}')
[ "$c" = 201 ] && pass "token can write (cluster-admin)" || fail "namespace create: $c $(cat "$W/out")"

c=$(code "$ADMIN" POST /apis/authentication.k8s.io/v1/tokenreviews \
  "{\"apiVersion\":\"authentication.k8s.io/v1\",\"kind\":\"TokenReview\",\"spec\":{\"token\":\"$T1\"}}")
if [ "$c" = 201 ] && grep -q '"authenticated":true' "$W/out" \
  && grep -q '"username":"system:admin"' "$W/out" && grep -q 'system:masters' "$W/out"; then
  pass "TokenReview: system:admin in system:masters"
else fail "TokenReview: $c $(cat "$W/out")"; fi

# The same token with its last character changed.
case "$T1" in *0) NM=${T1%?}1 ;; *) NM=${T1%?}0 ;; esac
c=$(code "$NM"); [ "$c" = 401 ] && pass "a near-miss token is refused" || fail "near miss: $c"
c=$(code "$ADMIN"); [ "$c" = 200 ] && pass "ServiceAccount-key JWT still works" || fail "JWT: $c"
grep -q "$T1" "$W/apiserver.log" && fail "the token appears in the apiserver log" || pass "token not logged"

# Rotation: the old token stops, the new one works.
line "$T2" >"$TOKFILE"
await "rewritten: new token accepted" "$T2" 200
c=$(code "$T1"); [ "$c" = 401 ] && pass "rewritten: old token revoked" || fail "old token after rewrite: $c"

# A malformed rewrite keeps the last good set.
echo "garbage-without-fields" >"$TOKFILE"
sleep 12
c=$(code "$T2"); [ "$c" = 200 ] && pass "malformed rewrite: last good token kept" || fail "after malformed: $c"
grep -q 'token auth file is malformed' "$W/apiserver.log" && pass "malformed file logged" || fail "malformed not logged"

# An install-config without apiToken removes the file: revoked.
rm -f "$TOKFILE"
await "file removed: token revoked" "$T2" 401

report
