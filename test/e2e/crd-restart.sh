#!/usr/bin/env bash
#
# Stored CRDs are served after a restart (#185), against a real apiserver on a
# real fastetcd:
#
#   test/e2e/crd-restart.sh         # from the checkout; builds what it runs
#
# 1. Two CRDs (one namespaced with two served versions, one cluster-scoped)
#    and a CR of each are created and served.
# 2. The apiserver restarts against the same store: groups are in /apis, the
#    resources in their group-version, the CRs readable, /readyz ok.
# 3. A reboot: datastore and apiserver both stop; the apiserver starts first
#    and the datastore 15 s later. Same checks once ready.
# 4. A second apiserver on the same store: a CRD created through it is served
#    by the first, and unserved there once deleted through it.
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

req() { # <method> <path> [json] [api] — prints the HTTP code, body in $W/out
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H 'Content-Type: application/json' "${4:-$API}$2" ${3:+-d "$3"}
}

crd() { # <group> <plural> <Kind> <scope> <versions-json>
  printf '{"apiVersion":"apiextensions.k8s.io/v1","kind":"CustomResourceDefinition",
  "metadata":{"name":"%s.%s"},"spec":{"group":"%s","scope":"%s",
  "names":{"plural":"%s","singular":"%s","kind":"%s"},"versions":%s}}' \
    "$2" "$1" "$1" "$4" "$2" "${2%s}" "$3" "$5"
}
V='{"name":"%s","served":true,"storage":%s,"schema":{"openAPIV3Schema":{"type":"object","x-kubernetes-preserve-unknown-fields":true}}}'
TWO="[$(printf "$V" v2 true),$(printf "$V" v2alpha1 false)]"
ONE="[$(printf "$V" v1 true)]"

start_apiserver() { # <port> <log> — sets STARTED_PID
  "$BIN/kube-apiserver" --bind-addr 127.0.0.1 --secure-port "$1" \
    --tls-cert-file "$W/apiserver.crt" --tls-private-key-file "$W/apiserver.key" \
    --etcd-servers http://127.0.0.1:$ETCD --anonymous-auth false \
    --service-account-signing-key-file "$W/sa.key" --service-account-key-file "$W/sa.pub" \
    >>"$2" 2>&1 &
  STARTED_PID=$!
}
wait_ready() { # <api> <pid> <what>
  for _ in $(seq 360); do
    kill -0 "$2" 2>/dev/null || { fail "$3: apiserver exited"; return 1; }
    curl -sfk -H "Authorization: Bearer $ADMIN" "$1/readyz" >/dev/null && return 0
    sleep 1
  done
  fail "$3: never ready"; return 1
}
start_store() {
  "$FASTETCD" --data-dir "$DATA" --listen-client-urls http://127.0.0.1:$ETCD \
    --listen-peer-urls http://127.0.0.1:$((ETCD + 1)) --listen-metrics-url 127.0.0.1:$((ETCD + 2)) \
    >>"$W/fastetcd.log" 2>&1 &
  STORE_PID=$!
}

served() { # <what> [api]
  local what=$1 api=${2:-$API} code
  req GET /apis "" "$api" >/dev/null
  for g in rk185.example.io rk185c.example.io; do
    grep -q "\"name\":\"$g\"" "$W/out" && pass "$what: $g in /apis" || fail "$what: $g missing from /apis"
  done
  for gv in rk185.example.io/v2 rk185.example.io/v2alpha1 rk185c.example.io/v1; do
    req GET "/apis/$gv" "" "$api" >/dev/null
    grep -q '"name":"\(gadgets\|clusterthings\)"' "$W/out" && pass "$what: $gv lists its resource" \
      || fail "$what: /apis/$gv: $(head -c 200 "$W/out")"
  done
  code=$(req GET /apis/rk185.example.io/v2/namespaces/default/gadgets/g1 "" "$api")
  [ "$code" = 200 ] && pass "$what: namespaced CR readable" || fail "$what: CR GET $code"
  code=$(req GET /apis/rk185.example.io/v2alpha1/gadgets "" "$api")
  [ "$code" = 200 ] && grep -q '"g1"' "$W/out" && pass "$what: CR listed at the second version" \
    || fail "$what: CR LIST $code"
  code=$(req GET /apis/rk185c.example.io/v1/clusterthings/c1 "" "$api")
  [ "$code" = 200 ] && pass "$what: cluster-scoped CR readable" || fail "$what: cluster CR GET $code"
}

# --- 1. create -----------------------------------------------------------------
code=$(req POST /apis/apiextensions.k8s.io/v1/customresourcedefinitions "$(crd rk185.example.io gadgets Gadget Namespaced "$TWO")")
[ "$code" = 201 ] || fail "create namespaced CRD: $code $(cat "$W/out")"
code=$(req POST /apis/apiextensions.k8s.io/v1/customresourcedefinitions "$(crd rk185c.example.io clusterthings ClusterThing Cluster "$ONE")")
[ "$code" = 201 ] || fail "create cluster CRD: $code $(cat "$W/out")"
code=$(req POST /apis/rk185.example.io/v2/namespaces/default/gadgets \
  '{"apiVersion":"rk185.example.io/v2","kind":"Gadget","metadata":{"name":"g1"},"spec":{"n":1}}')
[ "$code" = 201 ] || fail "create CR: $code $(cat "$W/out")"
code=$(req POST /apis/rk185c.example.io/v1/clusterthings \
  '{"apiVersion":"rk185c.example.io/v1","kind":"ClusterThing","metadata":{"name":"c1"}}')
[ "$code" = 201 ] || fail "create cluster CR: $code $(cat "$W/out")"
served "before restart"

# --- 2. apiserver restart --------------------------------------------------------
kill "$API_PID"; wait "$API_PID" 2>/dev/null
start_apiserver "$PORT" "$W/apiserver.log"; API_PID=$STARTED_PID
wait_ready "$API" "$API_PID" "apiserver restart" && served "apiserver restart"

# --- 3. reboot: both down, apiserver first --------------------------------------
kill "$API_PID"; wait "$API_PID" 2>/dev/null
kill "$STORE_PID"; wait "$STORE_PID" 2>/dev/null
start_apiserver "$PORT" "$W/apiserver.log"; API_PID=$STARTED_PID
sleep 15
start_store
wait_ready "$API" "$API_PID" "reboot" && served "reboot"
grep -q 'crd: registered [0-9]* stored' "$W/apiserver.log" && pass "boot logs how many CRDs it registered" \
  || fail "no CRD registration count in the apiserver log"

# --- 4. a second apiserver on the same store ------------------------------------
PORT2=$(python3 -c '
import pathlib, random, socket
lo, hi = map(int, pathlib.Path("/proc/sys/net/ipv4/ip_local_port_range").read_text().split())
for p in random.sample([p for p in range(20000, 32000) if not lo <= p <= hi], 200):
    s = socket.socket()
    try: s.bind(("127.0.0.1", p)); print(p); break
    except OSError: pass
    finally: s.close()
')
API2=https://127.0.0.1:$PORT2
start_apiserver "$PORT2" "$W/apiserver2.log"; API2_PID=$STARTED_PID
if wait_ready "$API2" "$API2_PID" "second apiserver"; then
  served "second apiserver" "$API2"
  code=$(req POST /apis/apiextensions.k8s.io/v1/customresourcedefinitions \
    "$(crd rk185o.example.io others Other Namespaced "$ONE")" "$API2")
  [ "$code" = 201 ] || fail "create CRD through the second apiserver: $code"
  ok=
  for _ in $(seq 20); do
    [ "$(req GET /apis/rk185o.example.io/v1/namespaces/default/others)" = 200 ] && { ok=1; break; }
    sleep 1
  done
  [ -n "$ok" ] && pass "CRD created through another apiserver is served here" \
    || fail "CRD created through another apiserver: still $(req GET /apis/rk185o.example.io/v1/namespaces/default/others) after 20 s"
  code=$(req DELETE /apis/apiextensions.k8s.io/v1/customresourcedefinitions/others.rk185o.example.io "" "$API2")
  [ "$code" = 200 ] || fail "delete CRD through the second apiserver: $code"
  ok=
  for _ in $(seq 20); do
    req GET /apis >/dev/null; grep -q rk185o.example.io "$W/out" || { ok=1; break; }
    sleep 1
  done
  [ -n "$ok" ] && pass "CRD deleted through another apiserver leaves /apis here" \
    || fail "deleted CRD's group still in /apis after 20 s"
  served "after the other replica's create and delete"
  kill "$API2_PID"
fi
[ "$FAIL" -ne 0 ] && { echo "---- second apiserver log (tail)"; tail -40 "$W/apiserver2.log" 2>/dev/null; }

report
