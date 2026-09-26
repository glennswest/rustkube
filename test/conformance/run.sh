#!/usr/bin/env bash
#
# The Kubernetes conformance suite against this control plane (#67).
#
#   test/conformance/run.sh [ginkgo focus regex]     # from the checkout
#
# Upstream's e2e.test, the release matching the API posture this apiserver
# reports (1.36), `[Conformance]` specs only, against a real kube-apiserver,
# kube-controller-manager and kube-scheduler on a fresh fastetcd (the setup is
# test/e2e/lib.sh).
#
# **There is no kubelet.** Two Nodes are API objects, kept Ready by renewing
# their Leases the way a kubelet would, so the scheduler places pods and the
# framework's node checks pass — but no pod ever runs. Every spec that needs a
# running pod fails on the pod-start timeout; that is expected here, and is
# what a run on real stormcos nodes is for. Specs that only need the API pass
# or fail on their merits.
#
# Output: one line per spec, `RESULT <passed|failed|skipped> <seconds> <name>`,
# followed by its first failure line — that is the triage input. The binaries
# are cached in $HOME/target/k8s-e2e on the build box.
set -u
cd "$(dirname "$0")/../.."
# shellcheck source=../e2e/lib.sh
. test/e2e/lib.sh
start_controller_manager
start_scheduler
FOCUS=${1:-'\[Conformance\]'}

# --- the suite ------------------------------------------------------------------
E2E=$HOME/target/k8s-e2e
V=$(curl -sfL https://dl.k8s.io/release/stable-1.36.txt) || exit 100
if [ ! -x "$E2E/$V/e2e.test" ]; then
  mkdir -p "$E2E/$V"
  curl -sfL "https://dl.k8s.io/$V/kubernetes-test-linux-amd64.tar.gz" \
    | tar -xz -C "$E2E/$V" --strip-components=3 kubernetes/test/bin/e2e.test kubernetes/test/bin/ginkgo \
    || exit 100
fi
echo "e2e.test $V"

cat >"$W/kubeconfig" <<KC
apiVersion: v1
kind: Config
clusters: [ { name: rk, cluster: { server: "$API", insecure-skip-tls-verify: true } } ]
users: [ { name: admin, user: { token: "$ADMIN" } } ]
contexts: [ { name: rk, context: { cluster: rk, user: admin } } ]
current-context: rk
KC
k() { curl -sk -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' "$@"; }

# --- two stand-in nodes, kept Ready -------------------------------------------
now() { date -u +%Y-%m-%dT%H:%M:%S.000000Z; }
for n in node-a node-b; do
  k -X POST "$API/api/v1/nodes" -d "{\"apiVersion\":\"v1\",\"kind\":\"Node\",\"metadata\":{\"name\":\"$n\",\"labels\":{\"kubernetes.io/hostname\":\"$n\",\"kubernetes.io/os\":\"linux\",\"kubernetes.io/arch\":\"amd64\"}}}" >/dev/null
  k -X PATCH -H 'Content-Type: application/merge-patch+json' "$API/api/v1/nodes/$n/status" -d "{\"status\":{
    \"capacity\":{\"cpu\":\"8\",\"memory\":\"16Gi\",\"pods\":\"110\",\"ephemeral-storage\":\"100Gi\"},
    \"allocatable\":{\"cpu\":\"8\",\"memory\":\"16Gi\",\"pods\":\"110\",\"ephemeral-storage\":\"100Gi\"},
    \"addresses\":[{\"type\":\"InternalIP\",\"address\":\"127.0.0.1\"},{\"type\":\"Hostname\",\"address\":\"$n\"}],
    \"nodeInfo\":{\"kubeletVersion\":\"$V\",\"operatingSystem\":\"linux\",\"architecture\":\"amd64\",\"containerRuntimeVersion\":\"none://0\"},
    \"conditions\":[{\"type\":\"Ready\",\"status\":\"True\",\"reason\":\"StandIn\",\"lastHeartbeatTime\":\"$(now | cut -c1-19)Z\"}]}}" >/dev/null
done
( while true; do
    for n in node-a node-b; do
      body="{\"apiVersion\":\"coordination.k8s.io/v1\",\"kind\":\"Lease\",\"metadata\":{\"name\":\"$n\",\"namespace\":\"kube-node-lease\"},\"spec\":{\"holderIdentity\":\"$n\",\"leaseDurationSeconds\":40,\"renewTime\":\"$(now)\"}}"
      k -X PUT "$API/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/$n" -d "$body" -o /dev/null -w '%{http_code}' | grep -q 200 \
        || k -X POST "$API/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases" -d "$body" >/dev/null
    done
    sleep 10
  done ) &

# --- run ------------------------------------------------------------------------
mkdir -p "$W/report"
"$E2E/$V/ginkgo" -p --procs=16 --timeout=3h --no-color --silence-skips \
  "$E2E/$V/e2e.test" -- \
  --kubeconfig="$W/kubeconfig" --provider=skeleton \
  --ginkgo.focus="$FOCUS" \
  --report-dir="$W/report" \
  --node-schedulable-timeout=2m \
  >"$W/e2e.log" 2>&1
echo "ginkgo exit $?"

# --- per-spec results, for triage ----------------------------------------------
python3 - "$W/report" <<'PY'
import glob, sys, xml.etree.ElementTree as ET
counts = {}
for f in sorted(glob.glob(sys.argv[1] + "/**/*.xml", recursive=True)):
    for tc in ET.parse(f).getroot().iter("testcase"):
        name = tc.get("name", "")
        if "[Conformance]" not in name:
            continue
        fail = tc.find("failure")
        err = tc.find("error")
        skip = tc.find("skipped")
        st = "failed" if (fail is not None or err is not None) else "skipped" if skip is not None else "passed"
        counts[st] = counts.get(st, 0) + 1
        print(f"RESULT {st} {float(tc.get('time', 0)):.0f} {name}")
        if st == "failed":
            node = fail if fail is not None else err
            msg = ((node.get("message") or "") + "\n" + (node.text or "")).strip()
            lines = [l.strip() for l in msg.splitlines() if l.strip()]
            print("  WHY " + (lines[0][:300] if lines else "(no message)"))
print("SUMMARY", " ".join(f"{k}={v}" for k, v in sorted(counts.items())))
PY
if grep -q "A BeforeSuite node failed" "$W/e2e.log"; then
  # The whole suite was skipped: the reason is in the setup, near the top.
  echo "---- BeforeSuite failed"
  grep -E -m 40 -i "error|fail|unable|timed out|not ready|forbidden" "$W/e2e.log" | cut -c1-300
fi
echo "---- e2e.log (tail)"; tail -15 "$W/e2e.log"
