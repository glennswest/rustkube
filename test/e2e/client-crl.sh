#!/usr/bin/env bash
#
# Revoked client certificates are refused (#260, stormcert#61). The apiserver
# runs with --client-ca-file (two signers, "nodes" and "other") and one
# --client-crl-file per signer: nodes' as PEM, other's as DER, the way
# stormcert writes them. Then, as stormcert would when a node leaves:
#
#   1. a nodes certificate is accepted, and so is a second one
#   2. the first is revoked and the CRL re-signed: refused on its next
#      handshake, no restart; the second still accepted
#   3. the CRL file goes missing, then holds garbage: logged, and the last
#      good CRL stays in force
#   4. other's DER CRL revoked its certificate from the start: refused
#   5. --client-crl-file without --client-ca-file: the apiserver does not start
#
# Files are looked at every 30 s, so this takes about 2 minutes.
# Exit status is the number of failed checks.
mkdir -p "$PWD/tmp"
CC=$(mktemp -d "$PWD/tmp/client-crl.XXXXXX")
# An openssl CA per signer: database, CRL number, a CRL valid 6 h.
mkca() { # <name>
  local d=$CC/$1
  mkdir -p "$d"; : >"$d/index.txt"; echo 1000 >"$d/crlnumber"
  printf '[ca]\ndefault_ca=rk\n[rk]\ndatabase=%s\ncrlnumber=%s\ndefault_md=sha256\ndefault_crl_hours=6\n' \
    "$d/index.txt" "$d/crlnumber" >"$d/ca.cnf"
  openssl req -x509 -newkey rsa:2048 -nodes -keyout "$d/ca.key" -out "$d/ca.crt" -days 2 \
    -subj "/CN=$1-ca" -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign 2>/dev/null
}
cert() { # <ca> <cn> <out-prefix>
  openssl req -newkey rsa:2048 -nodes -keyout "$3.key" -out "$3.csr" -subj "/CN=$2" 2>/dev/null
  openssl x509 -req -in "$3.csr" -CA "$CC/$1/ca.crt" -CAkey "$CC/$1/ca.key" -CAcreateserial \
    -out "$3.crt" -days 2 2>/dev/null
  rm -f "$3.csr"
}
gencrl() { # <ca> <out.pem>
  openssl ca -config "$CC/$1/ca.cnf" -gencrl -keyfile "$CC/$1/ca.key" -cert "$CC/$1/ca.crt" \
    -out "$2" 2>/dev/null
}
revoke() { # <ca> <cert>
  openssl ca -config "$CC/$1/ca.cnf" -revoke "$2" -keyfile "$CC/$1/ca.key" -cert "$CC/$1/ca.crt" 2>/dev/null
}
mkca nodes; mkca other
cat "$CC/nodes/ca.crt" "$CC/other/ca.crt" >"$CC/client-ca.crt"
cert nodes gone "$CC/gone"; cert nodes kept "$CC/kept"; cert other other "$CC/other-c"
gencrl nodes "$CC/nodes.crl.pem.new" && mv "$CC/nodes.crl.pem.new" "$CC/nodes.crl.pem"
revoke other "$CC/other-c.crt"; gencrl other "$CC/other.pem"
openssl crl -in "$CC/other.pem" -outform DER -out "$CC/other.crl"
[ -s "$CC/nodes.crl.pem" ] && [ -s "$CC/other.crl" ] || { echo "setup: no CRLs"; exit 100; }

export RK_APISERVER_ARGS="--client-ca-file $CC/client-ca.crt --client-crl-file $CC/nodes.crl.pem --client-crl-file $CC/other.crl"
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
trap 'cleanup; rm -rf "$CC"' EXIT

# Who a client certificate alone authenticates as; empty when refused.
whoami() { # <prefix>
  curl -s --cacert "$W/ca.crt" --cert "$CC/$1.crt" --key "$CC/$1.key" -X POST \
    -H 'Content-Type: application/json' "$API/apis/authentication.k8s.io/v1/selfsubjectreviews" \
    -d '{"apiVersion":"authentication.k8s.io/v1","kind":"SelfSubjectReview"}' \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"]["userInfo"]["username"])' 2>/dev/null
}
accepted() { [ "$(whoami "$1")" = "$2" ]; }
tick() { sleep 35; }

# 1.
grep -q 'client certificates checked against 2 CRL file(s)' "$W/apiserver.log" \
  && pass "started with both CRLs" || fail "no CRL startup line"
accepted gone gone && pass "nodes certificate 'gone' accepted" || fail "'gone' refused before revocation"
accepted kept kept && pass "nodes certificate 'kept' accepted" || fail "'kept' refused"

# 4. (from the start)
accepted other-c other && fail "other's revoked certificate accepted (DER CRL)" || pass "other's revoked certificate refused (DER CRL)"

# 2.
revoke nodes "$CC/gone.crt"
gencrl nodes "$CC/nodes.crl.pem.new" && mv "$CC/nodes.crl.pem.new" "$CC/nodes.crl.pem"
tick
accepted gone gone && fail "'gone' still accepted after revocation" || pass "'gone' refused after revocation, no restart"
accepted kept kept && pass "'kept' still accepted" || fail "'kept' refused after another's revocation"
grep -q 'client CRL reloaded without a restart' "$W/apiserver.log" && pass "CRL reload logged" || fail "no CRL reload logged"

# 3.
mv "$CC/nodes.crl.pem" "$CC/nodes.crl.pem.away"
tick
grep -q 'client CRL is unreadable, keeping the current one' "$W/apiserver.log" \
  && pass "missing CRL logged" || fail "missing CRL not logged"
accepted gone gone && fail "missing CRL: 'gone' accepted" || pass "missing CRL: 'gone' still refused"
printf 'not a crl' >"$CC/nodes.crl.pem"
tick
grep -q 'new client CRL is unusable, keeping the current one' "$W/apiserver.log" \
  && pass "garbage CRL logged" || fail "garbage CRL not logged"
accepted gone gone && fail "garbage CRL: 'gone' accepted" || pass "garbage CRL: 'gone' still refused"
accepted kept kept && pass "garbage CRL: 'kept' still accepted" || fail "garbage CRL: 'kept' refused"

# 5.
"$BIN/kube-apiserver" --bind-addr 127.0.0.1 --secure-port $((PORT + 1000)) \
  --tls-cert-file "$W/apiserver.crt" --tls-private-key-file "$W/apiserver.key" \
  --client-crl-file "$CC/other.crl" \
  --etcd-servers http://127.0.0.1:$ETCD --anonymous-auth false >"$W/apiserver-noca.log" 2>&1 &
pid=$!
for _ in $(seq 60); do kill -0 $pid 2>/dev/null || break; sleep 1; done
if kill -0 $pid 2>/dev/null; then
  fail "started with --client-crl-file and no --client-ca-file"; kill $pid
elif grep -q 'client-crl-file needs --client-ca-file' "$W/apiserver-noca.log"; then
  pass "--client-crl-file without --client-ca-file refused at start"
else
  fail "exited without the reason: $(tail -3 "$W/apiserver-noca.log")"
fi
wait $pid 2>/dev/null

report
