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
# - Always, a start the kubelet retries itself (#209; rustkube-node#76): the
#   VMI stays Pending with reason FailedStart; the VM reads CrashLoopBackOff,
#   not Starting, with the attempt's message on its Failure condition, the
#   VMI is not replaced, and Running clears it
# - template edits on a running VM (#269): sockets and memory.guest within
#   the VMI's maxSockets/maxGuest, a bridge NIC and a disk added, a hot-added
#   disk removed — all reach the VMI's spec, same uid; cores changed or the
#   boot disk removed → RestartRequired with the field, the VMI untouched;
#   putting the template back clears it
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
vmi_status() { # <name> <phase> [reason message] — the kubelet's write
  req GET "$VMI/$1" >/dev/null
  python3 - "$W/out" "$2" "${3:-E2E}" "${4:-e2e: guest ${2,,}}" >"$W/body" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
d["status"] = {"phase": sys.argv[2], "reason": sys.argv[3], "message": sys.argv[4]}
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
vm retrying Always
req POST $VM '{"apiVersion":"kubevirt.io/v1","kind":"VirtualMachine","metadata":{"name":"live"},
  "spec":{"runStrategy":"Always","template":{"spec":{
    "domain":{"cpu":{"cores":2,"sockets":1,"maxSockets":4},"memory":{"guest":"2Gi","maxGuest":"8Gi"},
      "devices":{"disks":[{"name":"root","disk":{"bus":"virtio"}}],
                 "interfaces":[{"name":"default","masquerade":{}}]}},
    "networks":[{"name":"default","pod":{}}],
    "volumes":[{"name":"root","persistentVolumeClaim":{"claimName":"root"}}]}}}}' >/dev/null
start_controller_manager

for n in always once retrying live; do
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

# Always, the kubelet retrying a failed start itself (#209).
req GET "$VMI/retrying" >/dev/null; uid3=$(jq_ "d['metadata']['uid']")
m1="start failed (attempt 1), retrying in 10s: e2e: multus network not supported"
vmi_status retrying Pending FailedStart "$m1" >/dev/null
wait_for 10 "$VM/retrying" "d['status']['printableStatus'] == 'CrashLoopBackOff'" \
  && pass "retrying: FailedStart VMI → CrashLoopBackOff, not Starting" || fail "retrying: $(cat "$W/out")"
[ "$(jq_ "[c['reason'] + ': ' + c['message'] for c in d['status']['conditions'] if c['type']=='Failure'][0]")" = "FailedStart: $m1" ] \
  && pass "retrying: the attempt and error on the Failure condition" || fail "retrying: conditions: $(jq_ "d['status'].get('conditions')")"
[ "$(jq_ "'startFailure' in d['status']")" = False ] \
  && pass "retrying: no startFailure (the kubelet owns the retry)" || fail "retrying: startFailure: $(jq_ "d['status'].get('startFailure')")"
m2="start failed (attempt 2), retrying in 20s: e2e: multus network not supported"
vmi_status retrying Pending FailedStart "$m2" >/dev/null
wait_for 10 "$VM/retrying" "[c['message'] for c in d['status']['conditions'] if c['type']=='Failure'] == ['$m2']" \
  && pass "retrying: the next attempt's message replaces it" || fail "retrying: $(cat "$W/out")"
sleep 12
req GET "$VMI/retrying" >/dev/null
[ "$(jq_ "d['metadata']['uid']")" = "$uid3" ] \
  && pass "retrying: VMI not replaced" || fail "retrying: VMI replaced: $(cat "$W/out")"
vmi_status retrying Running >/dev/null
wait_for 10 "$VM/retrying" "d['status']['printableStatus'] == 'Running' and not [c for c in d['status']['conditions'] if c['type']=='Failure']" \
  && pass "retrying: Running clears it" || fail "retrying: $(cat "$W/out")"


# Template edits on a running VM (#269). The kubelet's report: Running, the
# boot disk Ready.
vm_edit() { # <python statements on t, the template spec> — PUT the VM
  req GET "$VM/live" >/dev/null
  python3 - "$W/out" "$1" >"$W/body" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
t = d["spec"]["template"]["spec"]
exec(sys.argv[2])
print(json.dumps(d))
PY
  local code; code=$(req PUT "$VM/live" "$(cat "$W/body")")
  [ "$code" = 200 ] || fail "VM edit: $code $(cat "$W/out")"
}
vmi_report() { # <volumeStatus json>
  req GET "$VMI/live" >/dev/null
  python3 - "$W/out" "$1" >"$W/body" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
d["status"] = {"phase": "Running", "volumeStatus": json.loads(sys.argv[2])}
print(json.dumps(d))
PY
  req PUT "$VMI/live/status" "$(cat "$W/body")" >/dev/null
}
req GET "$VMI/live" >/dev/null; uid4=$(jq_ "d['metadata']['uid']")
vmi_report '[{"name":"root","phase":"Ready"}]'
vm_edit 't["domain"]["cpu"]["sockets"] = 3; t["domain"]["memory"]["guest"] = "6Gi"'
wait_for 15 "$VMI/live" "d['spec']['domain']['cpu']['sockets'] == 3 and d['spec']['domain']['memory']['guest'] == '6Gi'" \
  && pass "live: sockets and memory.guest reach the running VMI" || fail "live: cpu/memory: $(jq_ "d['spec']['domain']")"
[ "$(jq_ "d['metadata']['uid']")" = "$uid4" ] && pass "live: same VMI (not restarted)" || fail "live: VMI replaced"
vm_edit 't["domain"]["devices"]["interfaces"].append({"name": "lan", "bridge": {}})
t["networks"].append({"name": "lan", "multus": {"networkName": "lan"}})
t["domain"]["devices"]["disks"].append({"name": "data", "disk": {"bus": "scsi"}})
t["volumes"].append({"name": "data", "emptyDisk": {"capacity": "1Gi"}})'
wait_for 15 "$VMI/live" "[i['name'] for i in d['spec']['domain']['devices']['interfaces']] == ['default', 'lan'] and [x['name'] for x in d['spec']['volumes']] == ['root', 'data']" \
  && pass "live: a bridge NIC and a disk reach the running VMI" || fail "live: NIC/disk: $(jq_ "d['spec']")"
req GET "$VM/live" >/dev/null
[ "$(jq_ "[c for c in d['status'].get('conditions', []) if c['type'] == 'RestartRequired']")" = "[]" ] \
  && pass "live: no RestartRequired for live changes" || fail "live: $(jq_ "d['status']['conditions']")"
# The kubelet hot-added the disk: removing it is live.
vmi_report '[{"name":"root","phase":"Ready"},{"name":"data","phase":"Ready","target":"hp1","hotplugVolume":{}}]'
vm_edit 't["domain"]["devices"]["disks"] = [x for x in t["domain"]["devices"]["disks"] if x["name"] != "data"]
t["volumes"] = [x for x in t["volumes"] if x["name"] != "data"]'
wait_for 15 "$VMI/live" "[x['name'] for x in d['spec']['volumes']] == ['root']" \
  && pass "live: the hot-added disk leaves the running VMI" || fail "live: disk removal: $(jq_ "d['spec']['volumes']")"
# Not live: cores, and the disk it booted with.
vm_edit 't["domain"]["cpu"]["cores"] = 4
t["domain"]["devices"]["disks"] = []; t["volumes"] = []'
wait_for 15 "$VM/live" "any(c['type'] == 'RestartRequired' and c['status'] == 'True' and 'domain.cpu.cores' in c['message'] and 'root removed' in c['message'] for c in d['status']['conditions'])" \
  && pass "live: cores and a boot disk removed → RestartRequired naming them" || fail "live: $(jq_ "d['status'].get('conditions')")"
req GET "$VMI/live" >/dev/null
[ "$(jq_ "(d['spec']['domain']['cpu']['cores'], [x['name'] for x in d['spec']['volumes']], d['metadata']['uid'])")" = "(2, ['root'], '$uid4')" ] \
  && pass "live: the running VMI keeps its cores and boot disk" || fail "live: VMI: $(jq_ "d['spec']")"
vm_edit 't["domain"]["cpu"]["cores"] = 2
t["domain"]["devices"]["disks"] = [{"name": "root", "disk": {"bus": "virtio"}}]
t["volumes"] = [{"name": "root", "persistentVolumeClaim": {"claimName": "root"}}]'
wait_for 15 "$VM/live" "not [c for c in d['status']['conditions'] if c['type'] == 'RestartRequired']" \
  && pass "live: template put back → RestartRequired gone" || fail "live: $(jq_ "d['status'].get('conditions')")"

report
