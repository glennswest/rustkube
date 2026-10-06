#!/usr/bin/env bash
#
# Client certificates and the client CA roll without a restart (#105).
# kube-controller-manager and kube-scheduler authenticate with x509 client
# certificates from client CA "A"; then, as stormcert would:
#
#   1. the apiserver's --client-ca-file becomes a rollover bundle (A + B):
#      a B certificate is accepted on a new connection, without a restart
#   2. each component's pair is renewed from B in place, key first (a tick
#      sees the mismatch: refused once, still working), then the certificate
#   3. the bundle becomes B alone: an A certificate is refused
#   4. the apiserver restarts, so every connection is a new handshake: the
#      components still work — only possible with the renewed certificates
#
# A real apiserver, controller-manager and scheduler on fastetcd; stand-in
# Node. Files are looked at every 30 s, so this takes about 4 minutes.
# Exit status is the number of failed checks.
mkdir -p "$PWD/tmp"
CC=$(mktemp -d "$PWD/tmp/client-ca.XXXXXX")
cert() { # <ca-name> <cn> <out-prefix>
  openssl req -newkey rsa:2048 -nodes -keyout "$3.key" -out "$3.csr" -subj "/CN=$2" 2>/dev/null
  openssl x509 -req -in "$3.csr" -CA "$CC/$1.crt" -CAkey "$CC/$1.key" -CAcreateserial \
    -out "$3.crt" -days 2 2>/dev/null
  rm -f "$3.csr"
}
for ca in A B; do
  openssl req -x509 -newkey rsa:2048 -nodes -keyout "$CC/$ca.key" -out "$CC/$ca.crt" \
    -days 2 -subj "/CN=client-ca-$ca" 2>/dev/null
done
cp "$CC/A.crt" "$CC/client-ca.crt"
export RK_APISERVER_ARGS="--client-ca-file $CC/client-ca.crt"
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
trap 'cleanup; rm -rf "$CC"' EXIT

cert A system:kube-controller-manager "$CC/cm"
cert A system:kube-scheduler "$CC/sched"
cert A system:kube-controller-manager "$CC/probe-A"
cert B system:kube-controller-manager "$CC/probe-B"
"$BIN/kube-controller-manager" --apiserver "$API" --certificate-authority "$W/ca.crt" \
  --client-certificate "$CC/cm.crt" --client-key "$CC/cm.key" --leader-elect false >"$W/cm.log" 2>&1 &
"$BIN/kube-scheduler" --apiserver "$API" --certificate-authority "$W/ca.crt" \
  --client-certificate "$CC/sched.crt" --client-key "$CC/sched.key" --leader-elect false >"$W/sched.log" 2>&1 &

k() { curl -sf --cacert "$W/ca.crt" -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' "$@"; }
# A request authenticated by a client certificate alone.
as_cert() { # <prefix> <path>
  curl -sf --cacert "$W/ca.crt" --cert "$CC/$1.crt" --key "$CC/$1.key" -o /dev/null "$API$2"
}
tick() { sleep 35; }

# A stand-in Node kept alive by its Lease, and a Pod per check that only the
# scheduler can bind; a Deployment per check that only the controller-manager
# can turn into a ReplicaSet.
k -X POST "$API/api/v1/nodes" -d '{"apiVersion":"v1","kind":"Node","metadata":{"name":"n1","labels":{"kubernetes.io/hostname":"n1"}}}' >/dev/null
k -X PATCH -H 'Content-Type: application/merge-patch+json' "$API/api/v1/nodes/n1/status" \
  -d '{"status":{"capacity":{"cpu":"8","memory":"16Gi","pods":"110"},"allocatable":{"cpu":"8","memory":"16Gi","pods":"110"},"conditions":[{"type":"Ready","status":"True"}]}}' >/dev/null
k -X POST "$API/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases" \
  -d '{"apiVersion":"coordination.k8s.io/v1","kind":"Lease","metadata":{"name":"n1"},"spec":{"holderIdentity":"n1","leaseDurationSeconds":40}}' >/dev/null
( while sleep 10; do
    k -X PATCH -H 'Content-Type: application/merge-patch+json' \
      "$API/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/n1" \
      -d "{\"spec\":{\"renewTime\":\"$(date -u +%Y-%m-%dT%H:%M:%S.000000Z)\"}}" >/dev/null 2>&1
  done ) &

works() { # <label>: the controller-manager and the scheduler both act
  local n=$1 ok=
  k -X POST "$API/apis/apps/v1/namespaces/default/deployments" -d "{\"apiVersion\":\"apps/v1\",\"kind\":\"Deployment\",\"metadata\":{\"name\":\"d-$n\"},\"spec\":{\"replicas\":1,\"selector\":{\"matchLabels\":{\"app\":\"d-$n\"}},\"template\":{\"metadata\":{\"labels\":{\"app\":\"d-$n\"}},\"spec\":{\"containers\":[{\"name\":\"c\",\"image\":\"unused\"}]}}}}" >/dev/null
  k -X POST "$API/api/v1/namespaces/default/pods" -d "{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{\"name\":\"p-$n\"},\"spec\":{\"tolerations\":[{\"operator\":\"Exists\"}],\"containers\":[{\"name\":\"c\",\"image\":\"unused\"}]}}" >/dev/null
  for _ in $(seq 60); do
    k "$API/apis/apps/v1/namespaces/default/replicasets?labelSelector=app%3Dd-$n" | grep -q '"kind":"ReplicaSet"' && { ok=1; break; }
    sleep 1
  done
  [ -n "$ok" ] && pass "$n: controller-manager acts (Deployment → ReplicaSet)" || fail "$n: no ReplicaSet for d-$n"
  ok=
  for _ in $(seq 60); do
    k "$API/api/v1/namespaces/default/pods/p-$n" | grep -q '"nodeName":"n1"' && { ok=1; break; }
    sleep 1
  done
  [ -n "$ok" ] && pass "$n: scheduler acts (Pod bound)" || fail "$n: p-$n not bound"
}

works start
as_cert probe-A /api/v1/namespaces && pass "A certificate accepted" || fail "A certificate refused"
as_cert probe-B /api/v1/namespaces && fail "B certificate accepted before the CA rolled" || pass "B certificate refused before the CA rolled"

# 1. Rollover bundle.
cat "$CC/A.crt" "$CC/B.crt" >"$CC/client-ca.crt.new" && mv "$CC/client-ca.crt.new" "$CC/client-ca.crt"
tick
as_cert probe-B /api/v1/namespaces && pass "rollover bundle: B certificate accepted without a restart" || fail "rollover bundle: B certificate refused"
as_cert probe-A /api/v1/namespaces && pass "rollover bundle: A certificate still accepted" || fail "rollover bundle: A certificate refused"
grep -q 'client CA bundle reloaded without a restart' "$W/apiserver.log" && pass "apiserver logged the client CA reload" || fail "no client CA reload logged"

# 2. Renew both pairs from B, key first.
cert B system:kube-controller-manager "$CC/cm-new"
cert B system:kube-scheduler "$CC/sched-new"
cp "$CC/cm-new.key" "$CC/cm.key"; cp "$CC/sched-new.key" "$CC/sched.key"
tick
for c in cm sched; do
  n=$(grep -c 'new client certificate is unusable' "$W/$c.log")
  [ "$n" = 1 ] && pass "$c: key without its certificate refused once" || fail "$c: refusal logged $n times"
done
works mismatch
cp "$CC/cm-new.crt" "$CC/cm.crt"; cp "$CC/sched-new.crt" "$CC/sched.crt"
tick
for c in cm sched; do
  grep -q 'client certificate reloaded without a restart' "$W/$c.log" && pass "$c: renewed certificate loaded" || fail "$c: no reload logged"
done

# 3. B alone.
cp "$CC/B.crt" "$CC/client-ca.crt"
tick
as_cert probe-A /api/v1/namespaces && fail "B-only bundle: A certificate still accepted" || pass "B-only bundle: A certificate refused"
as_cert probe-B /api/v1/namespaces && pass "B-only bundle: B certificate accepted" || fail "B-only bundle: B certificate refused"

# 4. Every connection anew: restart the apiserver on the same store.
kill "$API_PID"; wait "$API_PID" 2>/dev/null
"$BIN/kube-apiserver" --bind-addr 127.0.0.1 --secure-port $PORT \
  --tls-cert-file "$W/apiserver.crt" --tls-private-key-file "$W/apiserver.key" \
  --etcd-servers http://127.0.0.1:$ETCD --anonymous-auth false \
  --service-account-signing-key-file "$W/sa.key" --service-account-key-file "$W/sa.pub" \
  $RK_APISERVER_ARGS >>"$W/apiserver.log" 2>&1 &
API_PID=$!
for _ in $(seq 120); do k "$API/readyz" >/dev/null && break; sleep 1; done
works renewed

report
