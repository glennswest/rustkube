#!/usr/bin/env bash
#
# The --tls self-signed certificate under --data-dir, with --cluster-domain in
# its SANs (#88): written on first start, the same certificate after a
# restart, regenerated when the cluster domain changes, and served from memory
# (with a warning) when the data dir cannot be written.
#
#   test/e2e/tls-data-dir.sh         # from the checkout; builds what it runs
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

P2=$((PORT + 1000))
DD=$W/data
pid=

# start <data-dir> <cluster-domain> <log>: a second apiserver with --tls on
# the rig's store; waits for its port.
start() {
  "$BIN/kube-apiserver" --bind-addr 127.0.0.1 --secure-port $P2 --tls \
    --data-dir "$1" --cluster-domain "$2" \
    --etcd-servers http://127.0.0.1:$ETCD --anonymous-auth false \
    --service-account-signing-key-file "$W/sa.key" --service-account-key-file "$W/sa.pub" \
    >"$3" 2>&1 &
  pid=$!
  for _ in $(seq 60); do
    openssl s_client -connect 127.0.0.1:$P2 </dev/null >/dev/null 2>&1 && return 0
    kill -0 $pid 2>/dev/null || return 1
    sleep 1
  done
  return 1
}
stop() { kill $pid 2>/dev/null; wait $pid 2>/dev/null; pid=; }
served() {
  openssl s_client -connect 127.0.0.1:$P2 -servername localhost </dev/null 2>/dev/null \
    | openssl x509 -noout -fingerprint -sha256 2>/dev/null
}
ondisk() { openssl x509 -in "$DD/apiserver.crt" -noout -fingerprint -sha256; }
has_san() { openssl x509 -in "$DD/apiserver.crt" -noout -ext subjectAltName | grep -q "DNS:$1\(,\|$\)"; }
# A handshake verified against the stored certificate alone.
trusted() {
  code=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$DD/apiserver.crt" \
    -H "Authorization: Bearer $ADMIN" "https://localhost:$P2/readyz")
  [ "$code" != 000 ]
}

# 1. First start: the pair is written, with the cluster domain's SAN.
start "$DD" example.org "$W/tls1.log" && pass "starts with --tls --data-dir" || fail "did not start: $(tail -3 "$W/tls1.log")"
[ -s "$DD/apiserver.crt" ] && [ -s "$DD/apiserver.key" ] && pass "apiserver.crt/.key written" || fail "no pair in $DD"
[ "$(stat -c %a "$DD/apiserver.key")" = 600 ] && pass "key is 0600" || fail "key mode $(stat -c %a "$DD/apiserver.key")"
has_san kubernetes.default.svc.example.org && pass "SAN kubernetes.default.svc.example.org" || fail "SANs: $(openssl x509 -in "$DD/apiserver.crt" -noout -ext subjectAltName)"
has_san kubernetes.default.svc.cluster.local && fail "still carries cluster.local" || pass "no hard-coded cluster.local"
FIRST=$(ondisk)
[ "$(served)" = "$FIRST" ] && pass "serves the stored certificate" || fail "served $(served), on disk $FIRST"
trusted && pass "a client trusting the file verifies the handshake" || fail "handshake not verified against the file"
stop

# 2. Restart: the same certificate.
start "$DD" example.org "$W/tls2.log" || fail "restart did not start"
[ "$(served)" = "$FIRST" ] && pass "restart serves the same certificate" || fail "restart served $(served), want $FIRST"
grep -q 'serving the self-signed certificate in' "$W/tls2.log" && pass "restart logged the reuse" || fail "no reuse logged"
stop

# 3. Another cluster domain: regenerated, with its SAN.
start "$DD" cluster.local "$W/tls3.log" || fail "domain change did not start"
SECOND=$(ondisk)
[ "$SECOND" != "$FIRST" ] && pass "changed --cluster-domain regenerates" || fail "certificate kept across a domain change"
has_san kubernetes.default.svc.cluster.local && pass "SAN kubernetes.default.svc.cluster.local" || fail "no cluster.local SAN"
[ "$(served)" = "$SECOND" ] && pass "serves the new certificate" || fail "served $(served), on disk $SECOND"
stop

# 4. A data dir that cannot be written: served from memory, with a warning.
: >"$W/notadir"
start "$W/notadir" cluster.local "$W/tls4.log" && pass "unwritable data dir: still serves" || fail "unwritable data dir: did not start: $(tail -3 "$W/tls4.log")"
grep -q 'serving it from memory' "$W/tls4.log" && pass "unwritable data dir: warned" || fail "no warning logged"
stop

report
