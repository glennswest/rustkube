#!/usr/bin/env bash
#
# Mint a long-lived kubelet bearer token for a cluster, signed offline with that
# cluster's ServiceAccount signing key.
#
#   ./gen-node-token.sh <sa.key> <node-name> [days]
#
# The apiserver validates SA tokens with RS256 against sa.pub (rustkube #11), so
# a JWT we sign here with sa.key authenticates just like one minted via
# TokenRequest — but without TokenRequest's 24h expiry, which would otherwise
# strand a kubelet after a day.
#
# Claims match what the apiserver's auth middleware expects:
#   sub    system:node:<name>     -> the node identity
#   groups [system:nodes]         -> bound by the bootstrap RBAC
set -euo pipefail

KEY="${1:-}"; NODE="${2:-}"; DAYS="${3:-3650}"
[ -n "$KEY" ] && [ -n "$NODE" ] || { echo "usage: $0 <sa.key> <node-name> [days]" >&2; exit 1; }
[ -s "$KEY" ] || { echo "no such key: $KEY" >&2; exit 1; }

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

now=$(date +%s)
exp=$((now + DAYS * 86400))

header='{"typ":"JWT","alg":"RS256"}'
payload=$(printf '{"sub":"system:node:%s","groups":["system:nodes"],"iat":%d,"exp":%d}' "$NODE" "$now" "$exp")

h=$(printf '%s' "$header"  | b64url)
p=$(printf '%s' "$payload" | b64url)
sig=$(printf '%s.%s' "$h" "$p" | openssl dgst -sha256 -sign "$KEY" -binary | b64url)

printf '%s.%s.%s\n' "$h" "$p" "$sig"
