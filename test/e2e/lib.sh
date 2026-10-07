# Shared setup for the end-to-end scripts: a real kube-apiserver (and
# optionally kube-controller-manager) on a fresh fastetcd, built from this
# checkout. Source it from the checkout root; it sets:
#
#   W        scratch dir, removed on exit
#   API      https://127.0.0.1:$PORT
#   BIN      where the rustkube binaries are
#   ADMIN    a system:masters bearer token; token <user> <groups-json> mints more
#   pass/fail, FAIL, report
#
# Needs cargo, git, openssl and curl, and network access to fetch fastetcd —
# unless it is handed prebuilt binaries, which is how the conformance VM runs
# it (no toolchain, no build slot):
#   RK_BIN       directory holding kube-apiserver, kube-controller-manager and
#                kube-scheduler (test/conformance/stage.sh builds and publishes it)
#   RK_FASTETCD_REF  source tag/branch (default in versions.sh)
#   RK_TOOLS     directory of prestaged upstream tools (oc, kubectl, CSI and
#                snapshot binaries, snapshot CRDs/RBAC) — the test image's
#                /rigs/tools (#173); without it a rig fetches what it needs
#   RK_FASTETCD  path to a fastetcd server binary
#   RK_RELEASE=1 build both release (as shipped) rather than debug, for timing
#   RK_APISERVER_ARGS  extra kube-apiserver flags (word-split; no spaces in values)
#   RK_ANONYMOUS_AUTH  the apiserver's --anonymous-auth (default false)
#   RK_CM_ARGS, RK_SCHED_ARGS  extra kube-controller-manager / kube-scheduler
#                flags (word-split)
#   RK_ETCD_MEMBERS  datastore members (default 1); 3 starts a Raft cluster on
#                loopback, member i on ports ETCD+3i (client), +1 (peer),
#                +2 (metrics); `start_member i` restarts one (#149)
#   RK_APISERVERS  apiserver ports to reserve (default 1); `start_apiserver n`
#                starts the nth on PORT+n against the same datastore (#149)
# Ports: 36443 (apiserver), 32379-32381 (fastetcd); RK_PORT_OFFSET=n adds n to
# each; without an override the rig chooses a free block outside the host
# ephemeral range just before starting servers (Linux /proc required).
set -u
# shellcheck source=versions.sh
. "$(dirname "${BASH_SOURCE[0]}")/versions.sh"
mkdir -p "$PWD/tmp"
: "${TMPDIR:=$PWD/tmp}"
export TMPDIR
W=$(mktemp -d)
cleanup() { kill $(jobs -p) 2>/dev/null; wait 2>/dev/null; rm -rf "$W"; }
trap cleanup EXIT

FAIL=0
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); }

# --- build (or take prebuilt binaries) ---------------------------------------
PROFILE=debug; [ -n "${RK_RELEASE:-}" ] && PROFILE=release
if [ -n "${RK_BIN:-}" ]; then
  BIN=$RK_BIN
  for b in kube-apiserver kube-controller-manager kube-scheduler; do
    [ -x "$BIN/$b" ] || { echo "RK_BIN=$BIN has no $b"; exit 100; }
  done
else
  cargo build -q ${RK_RELEASE:+--release} -p kube-apiserver -p kube-controller-manager -p kube-scheduler || exit 100
  BIN=${CARGO_TARGET_DIR:-$PWD/target}/$PROFILE
fi
if [ -n "${RK_FASTETCD:-}" ]; then
  FASTETCD=$RK_FASTETCD
  [ -x "$FASTETCD" ] || { echo "RK_FASTETCD=$FASTETCD is not executable"; exit 100; }
else
  git -c advice.detachedHead=false clone -q --depth 1 --branch "$RK_FASTETCD_REF" https://github.com/glennswest/fastetcd "$W/fastetcd" || exit 100
  echo "rig: fastetcd $(git -C "$W/fastetcd" rev-parse HEAD) ($RK_FASTETCD_REF)"
  (cd "$W/fastetcd" && cargo build -q ${RK_RELEASE:+--release} -p fastetcd-server) || exit 100
  FASTETCD=${CARGO_TARGET_DIR:-$W/fastetcd/target}/$PROFILE/fastetcd
fi
[ -x "$FASTETCD" ] || { echo "fastetcd build produced no executable"; exit 100; }

# --- credentials --------------------------------------------------------------
openssl genrsa -out "$W/sa.key" 2048 2>/dev/null
openssl rsa -in "$W/sa.key" -pubout -out "$W/sa.pub" 2>/dev/null
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
token() { # <user> <groups-json>
  local now h p s
  now=$(date +%s)
  h=$(printf '{"typ":"JWT","alg":"RS256"}' | b64url)
  # A day: a conformance chunk runs past the hour, and an expired token
  # turned its last specs into 401s.
  p=$(printf '{"sub":"%s","groups":%s,"iat":%d,"exp":%d}' "$1" "$2" "$now" $((now + 86400)) | b64url)
  s=$(printf '%s.%s' "$h" "$p" | openssl dgst -sha256 -sign "$W/sa.key" -binary | b64url)
  printf '%s.%s.%s' "$h" "$p" "$s"
}
ADMIN=$(token admin '["system:masters"]')
# The controller-manager and scheduler run as themselves, held to their
# bootstrap roles (#176), so every rig that starts them checks those roles.
# RK_CONTROL_PLANE_ADMIN=1 runs them as system:masters instead.
if [ -n "${RK_CONTROL_PLANE_ADMIN:-}" ]; then
  CM_TOKEN=$ADMIN SCHED_TOKEN=$ADMIN
else
  CM_TOKEN=$(token system:kube-controller-manager '[]')
  SCHED_TOKEN=$(token system:kube-scheduler '[]')
fi
export CM_TOKEN SCHED_TOKEN

# A throwaway CA and a serving cert from it, so the controllers verify the
# apiserver as they do in stormcos, and the root CA publisher has a real
# bundle to publish ($W/ca.crt).
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/ca.key" -out "$W/ca.crt" \
  -days 2 -subj /CN=rustkube-e2e-ca 2>/dev/null
openssl req -newkey rsa:2048 -nodes -keyout "$W/apiserver.key" -out "$W/apiserver.csr" \
  -subj /CN=apiserver 2>/dev/null
printf 'subjectAltName=IP:127.0.0.1,DNS:localhost,DNS:kubernetes,DNS:kubernetes.default.svc\n' >"$W/san.ext"
openssl x509 -req -in "$W/apiserver.csr" -CA "$W/ca.crt" -CAkey "$W/ca.key" -CAcreateserial \
  -out "$W/apiserver.crt" -days 2 -extfile "$W/san.ext" 2>/dev/null

if [ -n "${RK_PORT_OFFSET:-}" ]; then
  OFF=$RK_PORT_OFFSET
else
  OFF=$(RK_PORTS="$(seq -s ' ' 36443 $((36443 + ${RK_APISERVERS:-1} - 1))) $(seq -s ' ' 32379 $((32379 + 3 * ${RK_ETCD_MEMBERS:-1} - 1)))" python3 - <<'PORTS'
import os, pathlib, random, socket
ports = [int(p) for p in os.environ["RK_PORTS"].split()]
# Do not select a listener from Linux's outbound ephemeral range: another
# connection can claim it after this check and before the server binds.
ephemeral=pathlib.Path('/proc/sys/net/ipv4/ip_local_port_range').read_text().split()
lo,hi=map(int,ephemeral)
candidates=[offset for offset in range(-22000,16000)
    if all(not lo <= port+offset <= hi for port in ports)]
for offset in random.sample(candidates, min(100,len(candidates))):
    sockets = []
    try:
        for port in (p + offset for p in ports):
            sock = socket.socket(); sockets.append(sock); sock.bind(('127.0.0.1', port))
        print(offset); break
    except OSError: pass
    finally:
        for sock in sockets: sock.close()
else: raise SystemExit('no free test port block')
PORTS
  ) || exit 100
fi
PORT=$((36443 + OFF))
ETCD=$((32379 + OFF))
API=https://127.0.0.1:$PORT

# --- start --------------------------------------------------------------------
# Keep the datastore on the build's private disposable drive too.
DATA=$W/etcd
MEMBERS=${RK_ETCD_MEMBERS:-1}
if [ "$MEMBERS" = 1 ]; then
  ETCD_SERVERS=http://127.0.0.1:$ETCD
  "$FASTETCD" --data-dir "$DATA" --listen-client-urls http://127.0.0.1:$ETCD \
    --listen-peer-urls http://127.0.0.1:$((ETCD + 1)) --listen-metrics-url 127.0.0.1:$((ETCD + 2)) \
    >"$W/fastetcd.log" 2>&1 &
  STORE_PID=$!
  STORE_PIDS=("$STORE_PID")
else
  # A Raft cluster on loopback: member 0's data and log keep the single
  # member's names, so report() and the rigs read them unchanged.
  CLUSTER=$(for i in $(seq 0 $((MEMBERS - 1))); do printf 'm%d=http://127.0.0.1:%d,' "$i" $((ETCD + 3 * i + 1)); done)
  CLUSTER=${CLUSTER%,}
  ETCD_SERVERS=$(for i in $(seq 0 $((MEMBERS - 1))); do printf 'http://127.0.0.1:%d,' $((ETCD + 3 * i)); done)
  ETCD_SERVERS=${ETCD_SERVERS%,}
  STORE_PIDS=()
  start_member() { # <i> [existing]
    local i=$1 dir=$W/etcd log=$W/fastetcd.log
    [ "$i" = 0 ] || { dir=$W/etcd$i; log=$W/fastetcd$i.log; }
    "$FASTETCD" --name "m$i" --data-dir "$dir" \
      --listen-client-urls http://127.0.0.1:$((ETCD + 3 * i)) \
      --listen-peer-urls http://127.0.0.1:$((ETCD + 3 * i + 1)) \
      --initial-advertise-peer-urls http://127.0.0.1:$((ETCD + 3 * i + 1)) \
      --listen-metrics-url 127.0.0.1:$((ETCD + 3 * i + 2)) \
      --initial-cluster-token rig --initial-cluster "$CLUSTER" >>"$log" 2>&1 &
    STORE_PIDS[$i]=$!
  }
  for i in $(seq 0 $((MEMBERS - 1))); do start_member "$i"; done
  STORE_PID=${STORE_PIDS[0]}
fi
# fastetcd first: an apiserver that outwaits its 60s datastore gate boots
# into a hole.
for _ in $(seq 180); do
  kill -0 "$STORE_PID" 2>/dev/null || { cat "$W/fastetcd.log"; exit 100; }
  (exec 3<>/dev/tcp/127.0.0.1/$ETCD) 2>/dev/null && break
  sleep 1
done
(exec 3<>/dev/tcp/127.0.0.1/$ETCD) 2>/dev/null || { echo "fastetcd never listened"; tail -40 "$W/fastetcd.log"; exit 100; }
# The nth apiserver (0 is $API) on PORT+n, against every datastore member.
API_PIDS=()
start_apiserver() { # <n>
  local n=$1 log=$W/apiserver.log
  [ "$n" = 0 ] || log=$W/apiserver$n.log
  "$BIN/kube-apiserver" --bind-addr 127.0.0.1 --secure-port $((PORT + n)) \
    --tls-cert-file "$W/apiserver.crt" --tls-private-key-file "$W/apiserver.key" \
    --etcd-servers "$ETCD_SERVERS" --anonymous-auth "${RK_ANONYMOUS_AUTH:-false}" \
    --service-account-signing-key-file "$W/sa.key" --service-account-key-file "$W/sa.pub" \
    ${RK_APISERVER_ARGS:-} >>"$log" 2>&1 &
  API_PIDS[$n]=$!
}
start_apiserver 0
API_PID=${API_PIDS[0]}
# Six minutes: on a loaded build box the bootstrap writes alone have taken
# three.
ready=
for _ in $(seq 360); do
  kill -0 "$API_PID" 2>/dev/null || { cat "$W/apiserver.log"; exit 100; }
  curl -sfk -H "Authorization: Bearer $ADMIN" "$API/readyz" >/dev/null && { ready=1; break; }
  sleep 1
done
if [ -z "$ready" ]; then
  echo "apiserver never became ready"; cat "$W/apiserver.log"; exit 100
fi

# How fast the rig writes, said once, so a slow box is visible as that and
# not as a suite of timeouts.
t0=$(date +%s%N)
for i in $(seq 20); do
  curl -sk -o /dev/null -X POST -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' \
    "$API/api/v1/namespaces" -d "{\"apiVersion\":\"v1\",\"kind\":\"Namespace\",\"metadata\":{\"name\":\"rig-speed-$i\"}}"
done
echo "rig: 20 namespace creates in $(( ($(date +%s%N) - t0) / 1000000 )) ms (store at $DATA)"

start_controller_manager() {
  "$BIN/kube-controller-manager" --apiserver "$API" --token "$CM_TOKEN" \
    --certificate-authority "$W/ca.crt" --leader-elect false ${RK_CM_ARGS:-} >"$W/cm.log" 2>&1 &
}

start_scheduler() {
  "$BIN/kube-scheduler" --apiserver "$API" --token "$SCHED_TOKEN" \
    --certificate-authority "$W/ca.crt" --leader-elect false ${RK_SCHED_ARGS:-} >"$W/sched.log" 2>&1 &
}

# The number of failed checks is the exit status; logs when any failed.
report() {
  echo "---- $FAIL failed"
  if [ "$FAIL" -ne 0 ]; then
    echo "---- apiserver log (tail)"; tail -80 "$W/apiserver.log"
    echo "---- datastore log (tail)"; tail -80 "$W/fastetcd.log"
    echo "---- request/store counters at failure"
    curl --max-time 3 -sk -H "Authorization: Bearer $ADMIN" "$API/metrics" | grep -E '^(apiserver_current_inflight|apiserver_request_total|rustkube_store_)' || true
    # Without kubevirt's CRDs every namespace's VM LIST is a 404; that noise
    # would fill the tail.
    [ -f "$W/cm.log" ] && { echo "---- controller-manager log (tail, 404 LISTs dropped)"
      grep -v 'reflector LIST failed.*404 Not Found' "$W/cm.log" | tail -60;
      grep 'reconciling PDB membership' "$W/cm.log" | tail -30; }
  fi
  exit "$FAIL"
}
