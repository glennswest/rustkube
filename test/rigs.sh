#!/usr/bin/env bash
#
# The e2e rigs as a test suite (#173): `/test rigs` and `/test rigs-night`
# exec this, in rustkube's test image on a test machine, per stormcentral
# docs/test-standard.md. Every rig in test/e2e starts its own fastetcd and
# control plane inside this pod (loopback ports in the pod's own network
# namespace), from the commit's binaries that test/build.sh staged, and
# drives it. Nothing here touches the node's cluster: the run's namespace is
# not used.
#
#   rigs        the functional rigs, sized for the day budget (30 min)
#   rigs-night  the slow and timing-sensitive ones (idle watches, latency
#               under load, failover, cert reload ticks), night only
#
# One JSON line per rig — {"test": "rig:<name>", "status", "ms", "detail"} —
# and a summary; each rig's whole output goes to <results>/<name>.log
# (/results when writable). Exit 0 all passed, 1 a rig failed, 2 nothing
# could run. A rig's own exit is its number of failed checks, 100 when it
# could not start.
set -u
suite=${1:-${STORM_SUITE:-rigs}}

# Day: functional rigs, each a minute or two.
DAY="status-rv watch-timeout service-nodeport replication-controller compaction bad-token aggregation service-create metrics-auth hpa-metrics hpa-v1 limitrange table-review flowcontrol pod-resize endpointslice-mirroring scheduling-gates scale service-cidr field-validation admission-policy-api secret-stringdata cr-schema watch-deleted cache-reads metadata-watch wffc-latency dra-crud token-auth bound-token cr-status requester admission-webhook
     projects oc-adm vm-runstrategy vmi-launcher vmi-migration pod-limit daemonset-nodes
     indexed-selectors indexed-safety volume-expansion snapshot-controller"
# Night: idle and load windows, failover and reload ticks.
NIGHT="multi-master list-snapshot-race crd-restart deadlines scheduler-failover schedule-latency
       get-latency watch-deadline serving-cert client-cert-reload"

case "$suite" in
  rigs) list=$DAY ;;
  rigs-night) list=$NIGHT ;;
  *) echo "{\"test\": \"suite\", \"status\": \"fail\", \"ms\": 0, \"detail\": \"unknown suite $suite\"}"
     echo '{"summary": {"pass": 0, "fail": 1, "skip": 0}}'; exit 2 ;;
esac

json() { # <test> <status> <ms> <detail>
  python3 -I -c 'import json,sys; print(json.dumps({"test": sys.argv[1], "status": sys.argv[2], "ms": int(sys.argv[3]), "detail": sys.argv[4]}), flush=True)' "$@"
}
now_ms() { date +%s%3N; }

# Somewhere writable for the rigs' scratch, stores and logs.
scratch=${TMPDIR:-/tmp}
work=$(mktemp -d "$scratch/rigs.XXXXXX" 2>/dev/null) || {
  json setup fail 0 "no writable scratch directory ($scratch)"
  echo '{"summary": {"pass": 0, "fail": 1, "skip": 0}}'; exit 2; }
trap 'rm -rf "$work"' EXIT
cp -r /rigs/tree/. "$work/"
results=/results
mkdir -p "$results" 2>/dev/null && [ -w "$results" ] || results=$work/results
mkdir -p "$results"
export TMPDIR=$work/tmp RK_BIN=/rigs/bin RK_FASTETCD=/rigs/bin/fastetcd RK_TOOLS=/rigs/tools
export PATH=/rigs/tools:$PATH
mkdir -p "$TMPDIR"
cd "$work"
echo "rustkube rigs ($suite) at ${STORM_COMMIT:-?}; $(tr '\n' ';' </rigs/VERSIONS)" >&2

deadline=$(( $(date +%s) + ${STORM_TIMEOUT:-1800} - 60 ))
pass=0 fail=0 skip=0 broken=0 ran=0
for rig in $list; do
  left=$(( deadline - $(date +%s) ))
  if [ "$left" -lt 120 ]; then
    json "rig:$rig" skip 0 "not started: the suite's budget is spent"
    skip=$((skip + 1)); continue
  fi
  limit=$(( left < 1500 ? left : 1500 ))
  t0=$(now_ms)
  timeout -k 10 "$limit" bash "test/e2e/$rig.sh" >"$results/$rig.log" 2>&1
  rc=$?
  ms=$(( $(now_ms) - t0 ))
  ran=$((ran + 1))
  passed=$(grep -c '^PASS' "$results/$rig.log")
  if [ "$rc" = 0 ]; then
    json "rig:$rig" pass "$ms" "$passed checks"
    pass=$((pass + 1))
  else
    case $rc in
      100) why="could not start: $(grep -v '^\s*$' "$results/$rig.log" | tail -3 | tr '\n' ' ')"; broken=$((broken + 1)) ;;
      124|137) why="timed out after ${limit}s; $(grep '^FAIL' "$results/$rig.log" | head -3 | tr '\n' ' ')" ;;
      *) why="$rc failed, $passed passed: $(grep '^FAIL' "$results/$rig.log" | head -3 | tr '\n' ' ')" ;;
    esac
    json "rig:$rig" fail "$ms" "${why:0:1500}"
    fail=$((fail + 1))
  fi
done
echo "{\"summary\": {\"pass\": $pass, \"fail\": $fail, \"skip\": $skip}}"
# Nothing could start (every rig 100): the environment, not rustkube.
if [ "$ran" -gt 0 ] && [ "$broken" = "$ran" ]; then exit 2; fi
[ "$fail" = 0 ] && exit 0 || exit 1
