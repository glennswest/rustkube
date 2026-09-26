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
# Needs cargo, git, openssl and curl, and network access to fetch fastetcd.
# Ports: 36443 (apiserver), 32379-32381 (fastetcd).
set -u
mkdir -p "$HOME/tmp" && export TMPDIR="$HOME/tmp"
W=$(mktemp -d)
cleanup() { kill $(jobs -p) 2>/dev/null; wait 2>/dev/null; rm -rf "$W"; }
trap cleanup EXIT

PORT=36443
API=https://127.0.0.1:$PORT
FAIL=0
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); }

# --- build ------------------------------------------------------------------
export CARGO_TARGET_DIR=$HOME/target/rustkube-e2e
cargo build -q -p kube-apiserver -p kube-controller-manager -p kube-scheduler || exit 100
BIN=$CARGO_TARGET_DIR/debug
git clone -q --depth 1 https://github.com/glennswest/fastetcd "$W/fastetcd" || exit 100
(cd "$W/fastetcd" && CARGO_TARGET_DIR=$HOME/target/fastetcd-e2e cargo build -q -p fastetcd-server) || exit 100
FASTETCD=$(ls "$HOME"/target/fastetcd-e2e/debug/fastetcd* | grep -v '\.d$' | head -1)

# --- credentials --------------------------------------------------------------
openssl genrsa -out "$W/sa.key" 2048 2>/dev/null
openssl rsa -in "$W/sa.key" -pubout -out "$W/sa.pub" 2>/dev/null
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
token() { # <user> <groups-json>
  local now h p s
  now=$(date +%s)
  h=$(printf '{"typ":"JWT","alg":"RS256"}' | b64url)
  p=$(printf '{"sub":"%s","groups":%s,"iat":%d,"exp":%d}' "$1" "$2" "$now" $((now + 3600)) | b64url)
  s=$(printf '%s.%s' "$h" "$p" | openssl dgst -sha256 -sign "$W/sa.key" -binary | b64url)
  printf '%s.%s.%s' "$h" "$p" "$s"
}
ADMIN=$(token admin '["system:masters"]')

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

# --- start --------------------------------------------------------------------
# The store on tmpfs when there is one: this data is thrown away, and on a
# shared build box every fsync to disk queues behind everyone else's builds —
# namespace creation took over 30 s under the conformance suite's load (#67).
DATA=$W/etcd
if [ -d /dev/shm ] && [ -w /dev/shm ]; then
  DATA=$(mktemp -d /dev/shm/rustkube-e2e.XXXXXX)
  trap 'cleanup; rm -rf "$DATA"' EXIT
fi
"$FASTETCD" --data-dir "$DATA" --listen-client-urls http://127.0.0.1:32379 \
  --listen-peer-urls http://127.0.0.1:32380 --listen-metrics-url 127.0.0.1:32381 \
  >"$W/fastetcd.log" 2>&1 &
# fastetcd first: an apiserver that outwaits its 60s datastore gate boots
# into a hole.
for _ in $(seq 180); do
  (exec 3<>/dev/tcp/127.0.0.1/32379) 2>/dev/null && break
  sleep 1
done
(exec 3<>/dev/tcp/127.0.0.1/32379) 2>/dev/null || { echo "fastetcd never listened"; tail -40 "$W/fastetcd.log"; exit 100; }
"$BIN/kube-apiserver" --bind-addr 127.0.0.1 --secure-port $PORT \
  --tls-cert-file "$W/apiserver.crt" --tls-private-key-file "$W/apiserver.key" \
  --etcd-servers http://127.0.0.1:32379 --anonymous-auth false \
  --service-account-signing-key-file "$W/sa.key" --service-account-key-file "$W/sa.pub" \
  >"$W/apiserver.log" 2>&1 &
# Six minutes: on a loaded build box the bootstrap writes alone have taken
# three.
ready=
for _ in $(seq 360); do
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
  "$BIN/kube-controller-manager" --apiserver "$API" --token "$ADMIN" \
    --certificate-authority "$W/ca.crt" --leader-elect false >"$W/cm.log" 2>&1 &
}

start_scheduler() {
  "$BIN/kube-scheduler" --apiserver "$API" --token "$ADMIN" \
    --certificate-authority "$W/ca.crt" --leader-elect false >"$W/sched.log" 2>&1 &
}

# The number of failed checks is the exit status; logs when any failed.
report() {
  echo "---- $FAIL failed"
  if [ "$FAIL" -ne 0 ]; then
    echo "---- apiserver log (tail)"; tail -40 "$W/apiserver.log"
    [ -f "$W/cm.log" ] && { echo "---- controller-manager log (tail)"; tail -20 "$W/cm.log"; }
  fi
  exit "$FAIL"
}
