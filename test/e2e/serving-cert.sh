#!/usr/bin/env bash
#
# Serving-certificate reload against a real apiserver (#93): an openssl RSA
# pair on disk, renewed by deploy/renew-certs.sh, and the two ways a renewal
# used to leave a key beside a certificate it does not belong to — a signing
# failure in the script, and a key written without its certificate.
#
#   test/e2e/serving-cert.sh         # from the checkout; builds what it runs
#
# The apiserver looks at its files every 30 s, so this takes about 3 minutes.
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
ROOT=$PWD

# SHA-256 fingerprint of the certificate the apiserver hands out.
served() {
  openssl s_client -connect "127.0.0.1:$PORT" -servername localhost </dev/null 2>/dev/null \
    | openssl x509 -noout -fingerprint -sha256 2>/dev/null
}
ondisk() { openssl x509 -in "$W/apiserver.crt" -noout -fingerprint -sha256; }
# A verified handshake and an answered request, trusting only the rig's CA.
works() {
  curl -sf --cacert "$W/ca.crt" -o /dev/null -H "Authorization: Bearer $ADMIN" "$API/readyz"
}
files() { sha256sum "$W/apiserver.key" "$W/apiserver.crt" | cut -c1-64 | tr '\n' ' '; }
refusals() { grep -c 'private key does not match the certificate' "$W/apiserver.log"; }
# Past the next reload tick (30 s).
tick() { sleep 35; }

OLD=$(ondisk)
[ "$(served)" = "$OLD" ] && pass "serves the pair it started with" || fail "served $(served), on disk $OLD"
cp "$W/apiserver.key" "$W/apiserver.key.orig"
cp "$W/apiserver.crt" "$W/apiserver.crt.orig"

# 1. renew-certs.sh whose signing step fails: nothing on disk changes.
before=$(files)
cp "$W/ca.key" "$W/ca.key.orig"; echo garbage >"$W/ca.key"
FORCE=1 PKI=$W bash "$ROOT/deploy/renew-certs.sh" >"$W/renew-fail.out" 2>&1
rc=$?
mv "$W/ca.key.orig" "$W/ca.key"
[ "$rc" = 1 ] && pass "failed signing: renew-certs.sh exits 1" || fail "failed signing: exit $rc"
[ "$(files)" = "$before" ] && pass "failed signing: key and certificate untouched" \
  || { fail "failed signing: files changed"; cat "$W/renew-fail.out"; }
ls "$W"/apiserver.*.new "$W"/apiserver.csr >/dev/null 2>&1 \
  && fail "failed signing: scratch files left behind" || pass "failed signing: no scratch files left"

# 2. A key written without its certificate — what the old script left after a
#    failed signing, or any renewer seen between its two writes.
openssl genrsa -out "$W/apiserver.key" 2048 2>/dev/null
tick
works && pass "key without its certificate: TLS still works" || fail "key without its certificate: handshake fails"
[ "$(served)" = "$OLD" ] && pass "key without its certificate: old pair still served" || fail "served changed"
n=$(refusals)
[ "$n" = 1 ] && pass "mismatch logged" || fail "mismatch logged $n times, want 1"
tick
n=$(refusals)
[ "$n" = 1 ] && pass "mismatch not re-logged on an unchanged pair" || fail "mismatch logged $n times after a second tick"

# 3. The renewer finishes: the old key back (the pair matches again), then a
#    real renewal, which the apiserver picks up without a restart.
cp "$W/apiserver.key.orig" "$W/apiserver.key"
FORCE=1 PKI=$W bash "$ROOT/deploy/renew-certs.sh" >"$W/renew-ok.out" 2>&1
rc=$?
[ "$rc" = 0 ] && pass "renew-certs.sh renews (exit 0)" || { fail "renewal exit $rc"; cat "$W/renew-ok.out"; }
NEW=$(ondisk)
[ "$NEW" != "$OLD" ] && pass "a new certificate is on disk" || fail "certificate on disk unchanged"
openssl x509 -in "$W/apiserver.crt" -noout -ext subjectAltName | grep -q 'IP Address:127.0.0.1' \
  && pass "renewal kept the IP SAN" || fail "renewal dropped the IP SAN"
got=
for _ in $(seq 45); do
  got=$(served); [ "$got" = "$NEW" ] && break
  sleep 1
done
[ "$got" = "$NEW" ] && pass "renewed certificate served without a restart" || fail "still serving $got, want $NEW"
works && pass "renewed pair: verified handshake" || fail "renewed pair: handshake fails"
grep -q 'serving certificate reloaded without a restart' "$W/apiserver.log" \
  && pass "reload logged" || fail "reload not logged"

# 4. Startup refuses a mismatched pair rather than failing every handshake.
openssl genrsa -out "$W/stray.key" 2048 2>/dev/null
"$BIN/kube-apiserver" --bind-addr 127.0.0.1 --secure-port $((PORT + 1000)) \
  --tls-cert-file "$W/apiserver.crt" --tls-private-key-file "$W/stray.key" \
  --etcd-servers http://127.0.0.1:$ETCD --anonymous-auth false \
  --service-account-signing-key-file "$W/sa.key" --service-account-key-file "$W/sa.pub" \
  >"$W/apiserver2.log" 2>&1 &
pid=$!
for _ in $(seq 120); do kill -0 $pid 2>/dev/null || break; sleep 1; done
if kill -0 $pid 2>/dev/null; then
  fail "apiserver started on a mismatched pair"; kill $pid
elif grep -q 'does not match the certificate' "$W/apiserver2.log"; then
  pass "startup on a mismatched pair: refused, with the reason"
else
  fail "startup exited without the reason: $(tail -3 "$W/apiserver2.log")"
fi
wait $pid 2>/dev/null

report
