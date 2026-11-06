#!/usr/bin/env bash
#
# The ServiceAccount key files followed (#298, stormcos#176): a running
# apiserver has the cluster's pair installed over its own node's pair, as a
# promote onto a running master does (stormcluster#38/#67). It must sign with
# the new key, verify the new key's tokens, keep verifying the old key's until
# the next change, refuse a pair caught half written, and log each load's
# saPin (sha256 of the SPKI DER).
#
#   test/e2e/sa-key-reload.sh        # from the checkout; builds what it runs
#
# The files are re-read every 30 s, so the rig takes about two minutes.
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

# <key> <user> <groups-json>: a token signed offline with <key>, as lib.sh's
# token() signs with the node's own $W/sa.key.
token_with() {
  local now h p s
  now=$(date +%s)
  h=$(printf '{"typ":"JWT","alg":"RS256"}' | b64url)
  p=$(printf '{"sub":"%s","groups":%s,"iat":%d,"exp":%d}' "$2" "$3" "$now" $((now + 3600)) | b64url)
  s=$(printf '%s.%s' "$h" "$p" | openssl dgst -sha256 -sign "$1" -binary | b64url)
  printf '%s.%s.%s' "$h" "$p" "$s"
}
pin_of() { echo "sha256:$(openssl pkey -pubin -in "$1" -outform DER 2>/dev/null | sha256sum | cut -d' ' -f1)"; }
# <token> <public key file>: whether the token's signature is that key's.
signed_by() {
  local sig=$W/sig.bin
  printf '%s' "${1##*.}" | tr -- '-_' '+/' | awk '{ n = length($0) % 4; if (n) $0 = $0 substr("===", 1, 4 - n); print }' \
    | openssl base64 -d -A >"$sig"
  printf '%s' "${1%.*}" | openssl dgst -sha256 -verify "$2" -signature "$sig" >/dev/null 2>&1
}
code() { # <token> — HTTP code of a namespace list
  curl -sk -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $1" "$API/api/v1/namespaces"
}
# A token the apiserver itself signs: TokenRequest for default/rig.
minted() { # <admin token>
  curl -sk -X POST -H "Authorization: Bearer $1" -H 'Content-Type: application/json' \
    "$API/api/v1/namespaces/default/serviceaccounts/rig/token" \
    -d '{"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","spec":{}}' \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"]["token"])' 2>/dev/null
}
# <what> <grep pattern> <seconds>: wait for the apiserver's log to say it.
logged() {
  for _ in $(seq "$3"); do
    grep -qF -- "$2" "$W/apiserver.log" && { pass "$1"; return 0; }
    sleep 1
  done
  fail "$1: no log line matching $2 in $3 s"; return 1
}

NODE_PIN=$(pin_of "$W/sa.pub")
grep -q "signing saPin $NODE_PIN" "$W/apiserver.log" \
  && pass "start: the node's pair logged by its saPin" || fail "start: no 'signing saPin $NODE_PIN' in the log"

c=$(curl -sk -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' \
  "$API/api/v1/namespaces/default/serviceaccounts" -d '{"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"rig"}}')
[ "$c" = 201 ] || fail "ServiceAccount default/rig: $c"
T1=$(minted "$ADMIN")
signed_by "$T1" "$W/sa.pub" && pass "before: TokenRequest signed with the node's key" || fail "before: TokenRequest not signed with sa.pub"

# The cluster's pair, generated elsewhere (the seed).
openssl genrsa -out "$W/cluster.key" 2048 2>/dev/null
openssl rsa -in "$W/cluster.key" -pubout -out "$W/cluster.pub" 2>/dev/null
CLUSTER_PIN=$(pin_of "$W/cluster.pub")
CLUSTER_ADMIN=$(token_with "$W/cluster.key" admin '["system:masters"]')
c=$(code "$CLUSTER_ADMIN"); [ "$c" = 401 ] && pass "before: a cluster-key token is refused" || fail "before: cluster-key token $c"

# Half a pair: the private key replaced, the public file not yet. Refused, and
# the node's key keeps signing.
cp "$W/sa.key" "$W/node.key"; cp "$W/sa.pub" "$W/node.pub"
cp "$W/cluster.key" "$W/sa.key.new" && mv "$W/sa.key.new" "$W/sa.key"
logged "half a pair: refused, current keys kept" "new ServiceAccount key pair is unusable" 45
T_HALF=$(minted "$ADMIN")
signed_by "$T_HALF" "$W/node.pub" && pass "half a pair: still signing with the node's key" \
  || fail "half a pair: TokenRequest not signed with the node's key"

# The other half: the pair is complete, and applies without a restart.
cp "$W/cluster.pub" "$W/sa.pub.new" && mv "$W/sa.pub.new" "$W/sa.pub"
logged "whole pair: reloaded, logged by the cluster's saPin" "signing saPin $CLUSTER_PIN" 45
kill -0 "$API_PID" 2>/dev/null && pass "the same apiserver process (no restart)" || fail "the apiserver exited"
T2=$(minted "$CLUSTER_ADMIN")
signed_by "$T2" "$W/cluster.pub" && pass "after: TokenRequest signed with the cluster's key" \
  || fail "after: TokenRequest not signed with the cluster's key"
c=$(code "$CLUSTER_ADMIN"); [ "$c" = 200 ] && pass "after: a cluster-key token authenticates" || fail "after: cluster-key token $c"
c=$(code "$ADMIN"); [ "$c" = 200 ] && pass "after: the node's key's tokens still verify (until the next change)" \
  || fail "after: node-key token $c"
c=$(code "$T1"); [ "$c" != 401 ] && pass "after: a pod token signed before the swap still authenticates ($c)" \
  || fail "after: the earlier TokenRequest token is 401"

# The next change (another key added beside the cluster's) drops the node's.
openssl genrsa 2048 2>/dev/null | openssl rsa -pubout 2>/dev/null >"$W/extra.pub"
cat "$W/cluster.pub" "$W/extra.pub" >"$W/sa.pub.new" && mv "$W/sa.pub.new" "$W/sa.pub"
logged "next change: reloaded with two verification keys" "verifying [\"$CLUSTER_PIN\", \"$(pin_of "$W/extra.pub")\"]" 45
c=$(code "$ADMIN"); [ "$c" = 401 ] && pass "next change: the node's key's tokens stop (401)" || fail "next change: node-key token $c"
c=$(code "$CLUSTER_ADMIN"); [ "$c" = 200 ] && pass "next change: the cluster key's tokens still work" || fail "next change: cluster-key token $c"

ADMIN=$CLUSTER_ADMIN report
