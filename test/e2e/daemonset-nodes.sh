#!/usr/bin/env bash
#
# DaemonSet indexed workers (#146), against a real apiserver on a real
# fastetcd with the controller-manager running: Node membership and label
# changes drive DaemonSet pods without a poll pass. A node added, relabelled
# or deleted is acted on for exactly the DaemonSets whose nodeSelector matches
# it; a node heartbeat changes nothing.
#
#   test/e2e/daemonset-nodes.sh     # from the checkout; builds what it runs
#
# Latencies are printed, and a check fails past RK_DS_LIMIT_MS (default
# 3000): the old whole-cluster pass alone waited 2 s between passes.
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
LIMIT=${RK_DS_LIMIT_MS:-3000}

req() { # <method> <path> [json] — prints the HTTP code
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H 'Content-Type: application/json' "$API$2" ${3:+-d "$3"}
}
node() { # <name> <labels-json>
  req POST /api/v1/nodes "{\"apiVersion\":\"v1\",\"kind\":\"Node\",\"metadata\":{\"name\":\"$1\",\"labels\":$2},\"status\":{\"conditions\":[{\"type\":\"Ready\",\"status\":\"True\",\"lastHeartbeatTime\":\"$(date -u +%FT%TZ)\"}]}}"
}
daemonset() { # <name> <nodeSelector-json>
  req POST /apis/apps/v1/namespaces/default/daemonsets "{\"apiVersion\":\"apps/v1\",\"kind\":\"DaemonSet\",\"metadata\":{\"name\":\"$1\"},\"spec\":{\"selector\":{\"matchLabels\":{\"ds\":\"$1\"}},\"template\":{\"metadata\":{\"labels\":{\"ds\":\"$1\"}},\"spec\":{\"nodeSelector\":$2,\"containers\":[{\"name\":\"c\",\"image\":\"busybox\"}]}}}}"
}
# The sorted nodes holding a live (not deleting) pod of DaemonSet $1.
placed() {
  req GET /api/v1/namespaces/default/pods >/dev/null
  python3 - "$W/out" "$1" <<'PY'
import json, sys
items = json.load(open(sys.argv[1])).get("items", [])
print(" ".join(sorted(
    p["spec"].get("nodeName", "") for p in items
    if not p["metadata"].get("deletionTimestamp")
    and any(r.get("name") == sys.argv[2] and r.get("kind") == "DaemonSet"
            for r in p["metadata"].get("ownerReferences", [])))))
PY
}
# <what> <daemonset> <expected nodes>: wait for it, print how long it took.
expect() {
  local t0 got ms
  t0=$(date +%s%N)
  for _ in $(seq 300); do
    got=$(placed "$2")
    [ "$got" = "$3" ] && break
    sleep 0.05
  done
  ms=$(( ($(date +%s%N) - t0) / 1000000 ))
  if [ "$got" != "$3" ]; then
    fail "$1: $2 on [$got], want [$3]"
  elif [ "$ms" -gt "$LIMIT" ]; then
    fail "$1: $2 on [$3] after ${ms} ms (limit ${LIMIT} ms)"
  else
    pass "$1: $2 on [$3] in ${ms} ms"
  fi
}

start_controller_manager
node n1 '{}' >/dev/null
node n2 '{"gpu":"yes"}' >/dev/null
daemonset all '{}' >/dev/null
daemonset gpu '{"gpu":"yes"}' >/dev/null
# The first placement includes the controller-manager's startup and initial
# LIST, so it gets the lib's generous budget rather than the latency limit.
LIMIT=60000 expect "initial placement" all "n1 n2"
expect "initial placement" gpu "n2"

node n3 '{"gpu":"yes"}' >/dev/null
expect "node added" all "n1 n2 n3"
expect "node added" gpu "n2 n3"

# Pod creation precedes the status write, which describes the worker's input
# snapshot. Wait for the follow-up reconciliation to count all three Pods
# before measuring heartbeat-only writes; placement alone is not convergence.
settled=
for _ in $(seq 100); do
  req GET /apis/apps/v1/namespaces/default/daemonsets/all >/dev/null
  if python3 - "$W/out" <<'PY_STATUS'
import json, sys
ds=json.load(open(sys.argv[1]))
expected=dict(desiredNumberScheduled=3,currentNumberScheduled=3,
    updatedNumberScheduled=3,numberReady=0,numberAvailable=0,
    numberUnavailable=3,numberMisscheduled=0,
    observedGeneration=ds['metadata'].get('generation',1))
sys.exit(0 if ds.get('status') == expected else 1)
PY_STATUS
  then settled=1; break; fi
  sleep 0.1
done
[ -n "$settled" ] || { fail "status did not converge before heartbeat check"; cat "$W/out"; report; }
cp "$W/out" "$W/ds-before.json"
# Heartbeats: status-only node writes. Neither DaemonSet's pods or status may
# change (an unchanged status is not rewritten, so its resourceVersion holds).
before=$(python3 -c "import json;print(json.load(open('$W/out'))['metadata']['resourceVersion'])")
for i in 1 2 3 4 5; do
  req GET /api/v1/nodes/n1 >/dev/null
  python3 - "$W/out" "$i" >"$W/body" <<'PY'
import json, sys
n = json.load(open(sys.argv[1]))
n["status"]["conditions"][0]["lastHeartbeatTime"] = "2026-01-01T00:00:0%sZ" % sys.argv[2]
print(json.dumps(n))
PY
  curl -sk -o /dev/null -X PUT -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' \
    "$API/api/v1/nodes/n1/status" -d @"$W/body"
done
sleep 2
req GET /apis/apps/v1/namespaces/default/daemonsets/all >/dev/null
after=$(python3 -c "import json;print(json.load(open('$W/out'))['metadata']['resourceVersion'])")
[ "$before" = "$after" ] && pass "heartbeats: DaemonSet not rewritten (rv $after)" \
  || { fail "heartbeats: DaemonSet rewritten ($before -> $after)"; cat "$W/ds-before.json" "$W/out"; }
[ "$(placed all)" = "n1 n2 n3" ] && pass "heartbeats: pods unchanged" || fail "heartbeats: pods moved: $(placed all)"

# Relabel n2 away from gpu: gpu's pod there is now misplaced and deleted.
req GET /api/v1/nodes/n2 >/dev/null
python3 -c "import json;n=json.load(open('$W/out'));n['metadata']['labels']={};print(json.dumps(n))" >"$W/body"
curl -sk -o /dev/null -X PUT -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' \
  "$API/api/v1/nodes/n2" -d @"$W/body"
expect "label removed" gpu "n3"
expect "label removed" all "n1 n2 n3"

req DELETE /api/v1/nodes/n1 >/dev/null
expect "node deleted" all "n2 n3"

# Status follows: desired = eligible nodes.
for _ in $(seq 60); do
  req GET /apis/apps/v1/namespaces/default/daemonsets/all >/dev/null
  desired=$(python3 -c "import json;print(json.load(open('$W/out')).get('status',{}).get('desiredNumberScheduled'))")
  [ "$desired" = 2 ] && break
  sleep 0.1
done
[ "$desired" = 2 ] && pass "status: desiredNumberScheduled 2" || fail "status: desiredNumberScheduled $desired"

report
