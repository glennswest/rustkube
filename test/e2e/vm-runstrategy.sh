#!/usr/bin/env bash
#
# The VirtualMachine controller honours runStrategy on a failed VMI (#104),
# against a real apiserver + controller-manager on a real fastetcd, with no
# node: the test plays the kubelet by writing the VMI's status.
#
#   test/e2e/vm-runstrategy.sh      # from the checkout; builds what it runs
#
# - Always: a Failed VMI makes the VM read CrashLoopBackOff, and after the
#   first 10 s backoff it is replaced by a new VMI (new uid);
#   status.startFailure counts it
# - Once: a Failed VMI is left, the VM reads Failed, and the VMI's message
#   is on the VM's Failure condition
# - ready follows the VMI: Running → ready true, printableStatus Running
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

req() { # <method> <path> [json] — prints the HTTP code, body in $W/out
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H 'Content-Type: application/json' "$API$2" ${3:+-d "$3"}
}
jq_() { python3 -c "import json,sys; d=json.load(open('$W/out')); print(eval(sys.argv[1]))" "$1" 2>/dev/null; }

crd() { # <plural> <kind> <short>
  req POST /apis/apiextensions.k8s.io/v1/customresourcedefinitions "{
    \"apiVersion\":\"apiextensions.k8s.io/v1\",\"kind\":\"CustomResourceDefinition\",
    \"metadata\":{\"name\":\"$1.kubevirt.io\"},
    \"spec\":{\"group\":\"kubevirt.io\",\"scope\":\"Namespaced\",
      \"names\":{\"plural\":\"$1\",\"singular\":\"$3\",\"kind\":\"$2\",\"listKind\":\"$2List\"},
      \"versions\":[{\"name\":\"v1\",\"served\":true,\"storage\":true,
        \"subresources\":{\"status\":{}},
        \"schema\":{\"openAPIV3Schema\":{\"type\":\"object\",\"x-kubernetes-preserve-unknown-fields\":true}}}]}}" >/dev/null
}
crd virtualmachines VirtualMachine virtualmachine
crd virtualmachineinstances VirtualMachineInstance virtualmachineinstance
for _ in $(seq 30); do
  [ "$(req GET /apis/kubevirt.io/v1/namespaces/default/virtualmachines)" = 200 ] && break
  sleep 1
done

VM=/apis/kubevirt.io/v1/namespaces/default/virtualmachines
VMI=/apis/kubevirt.io/v1/namespaces/default/virtualmachineinstances
vm() { # <name> <runStrategy>
  req POST $VM "{\"apiVersion\":\"kubevirt.io/v1\",\"kind\":\"VirtualMachine\",
    \"metadata\":{\"name\":\"$1\"},\"spec\":{\"runStrategy\":\"$2\",
    \"template\":{\"spec\":{\"domain\":{\"devices\":{}}}}}}" >/dev/null
}
vmi_status() { # <name> <phase> — the kubelet's write
  req GET "$VMI/$1" >/dev/null
  python3 - "$W/out" "$2" >"$W/body" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
d["status"] = {"phase": sys.argv[2], "reason": "E2E", "message": "e2e: guest " + sys.argv[2].lower()}
print(json.dumps(d))
PY
  req PUT "$VMI/$1/status" "$(cat "$W/body")"
}
wait_for() { # <seconds> <path> <python expr on d> — true once it holds
  local n=$1 p=$2 e=$3
  for _ in $(seq "$n"); do
    [ "$(req GET "$p")" = 200 ] && [ "$(jq_ "$e")" = True ] && return 0
    sleep 1
  done
  return 1
}

vm always Always
vm once Once
start_controller_manager

for n in always once; do
  wait_for 60 "$VMI/$n" "d['metadata']['uid'] != ''" && pass "$n: VMI created" \
    || { fail "$n: no VMI"; report; }
done

# Ready follows the VMI.
code=$(vmi_status always Running)
[ "$code" = 200 ] || fail "VMI status write: $code $(cat "$W/out")"
wait_for 20 "$VM/always" "d['status']['ready'] is True and d['status']['printableStatus'] == 'Running'" \
  && pass "always: Running VMI → ready, Running" || fail "always: not ready: $(cat "$W/out")"

# Always: failed → CrashLoopBackOff, then replaced after the backoff.
req GET "$VMI/always" >/dev/null; uid1=$(jq_ "d['metadata']['uid']")
vmi_status always Failed >/dev/null
wait_for 8 "$VM/always" "d['status']['printableStatus'] == 'CrashLoopBackOff'" \
  && pass "always: failed VMI → CrashLoopBackOff" || fail "always: $(cat "$W/out")"
[ "$(jq_ "d['status']['startFailure']['consecutiveFailCount']")" = 1 ] \
  && pass "always: startFailure counts 1" || fail "always: startFailure: $(jq_ "d['status'].get('startFailure')")"
wait_for 40 "$VMI/always" "d['metadata']['uid'] not in ('', '$uid1')" \
  && pass "always: failed VMI replaced (new uid)" || fail "always: VMI not replaced: $(cat "$W/out")"
wait_for 10 "$VM/always" "d['status']['printableStatus'] == 'Starting'" \
  && pass "always: replacement reads Starting" || fail "always: $(cat "$W/out")"

# Once: failed → left, Failed, message on the Failure condition.
req GET "$VMI/once" >/dev/null; uid2=$(jq_ "d['metadata']['uid']")
vmi_status once Failed >/dev/null
wait_for 10 "$VM/once" "d['status']['printableStatus'] == 'Failed'" \
  && pass "once: failed VMI → Failed" || fail "once: $(cat "$W/out")"
[ "$(jq_ "[c['message'] for c in d['status']['conditions'] if c['type']=='Failure'][0]")" = "e2e: guest failed" ] \
  && pass "once: VMI message on the Failure condition" || fail "once: conditions: $(jq_ "d['status'].get('conditions')")"
sleep 15
req GET "$VMI/once" >/dev/null
[ "$(jq_ "d['metadata']['uid']")" = "$uid2" ] && [ "$(jq_ "d['status']['phase']")" = Failed ] \
  && pass "once: failed VMI left in place" || fail "once: VMI changed: $(cat "$W/out")"

report
