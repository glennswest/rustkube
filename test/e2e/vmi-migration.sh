#!/usr/bin/env bash
#
# VirtualMachineInstanceMigration (#184) on a real apiserver, controller-manager
# and scheduler over a real fastetcd. There is no kubelet: the rig plays the
# target kubelet (writes targetNodeAddress) and the source kubelet (writes
# startTimestamp, then completed or failed) through the VMI's
# status.migrationState, as rustkube-node#40 will.
#
#   test/e2e/vmi-migration.sh       # from the checkout; builds what it runs
#
# - `virtctl migrate` (PUT virtualmachines/{vm}/migrate) creates a migration;
#   a second one while it is in flight is refused (409)
# - the scheduler picks the target: not the source, not a node too small
# - phases Scheduling → PreparingTarget → TargetReady → Running → Succeeded
#   follow the VMI, and success moves the VMI's status.nodeName
# - the target is charged while the migration is in flight: a second 2Gi VM
#   cannot use it, and lands once the source is freed
# - the source reporting failure fails the migration and leaves the VMI
# - deleting a migration before it sends aborts it on the VMI
# - no node for the target: TargetScheduled=False on the migration; a node
#   freed later takes it
# - addedNodeSelector (#208): carried onto the migration, it forces the
#   second-choice node; one no node meets is TargetScheduled=False; a
#   non-string value is 422
# - a migration of a VMI that does not exist fails
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
start_scheduler
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, time, urllib.parse, datetime

API = urllib.parse.urlparse(os.environ["API"]); TOKEN = os.environ["ADMIN"]
CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
def call(method, path, body=None, ctype="application/json"):
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + TOKEN, "Content-Type": ctype})
    r = conn.getresponse(); data = r.read()
    try:
        return r.status, json.loads(data)
    except ValueError:
        return r.status, {}
def req(method, path, body=None, ctype="application/json"):
    code, out = call(method, path, body, ctype)
    if code >= 300 and code != 409:
        print(f"setup {method} {path}: {code} {json.dumps(out)[:300]}"); sys.exit(100)
    return out
def merge(path, body):
    return call("PATCH", path, body, "application/merge-patch+json")
def until(f, limit=30):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        v = f()
        if v: return v
        time.sleep(0.2)
    return f()

def crd(plural, kind):
    req("POST", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions", {
        "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
        "metadata": {"name": f"{plural}.kubevirt.io"},
        "spec": {"group": "kubevirt.io", "scope": "Namespaced",
                 "names": {"plural": plural, "singular": plural[:-1], "kind": kind, "listKind": kind + "List"},
                 "versions": [{"name": "v1", "served": True, "storage": True,
                               "subresources": {"status": {}},
                               "schema": {"openAPIV3Schema": {"type": "object",
                                          "x-kubernetes-preserve-unknown-fields": True}}}]}})
for plural, kind in [("virtualmachines", "VirtualMachine"),
                     ("virtualmachineinstances", "VirtualMachineInstance"),
                     ("virtualmachineinstancemigrations", "VirtualMachineInstanceMigration")]:
    crd(plural, kind)
until(lambda: call("GET", "/apis/kubevirt.io/v1/namespaces/default/virtualmachineinstancemigrations")[0] == 200)

later = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=2)).strftime("%Y-%m-%dT%H:%M:%S.000000Z")
def node(name, memory, labels={}):
    n = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node",
        "metadata": {"name": name, "labels": dict({"kubernetes.io/hostname": name}, **labels)}})
    res = {"cpu": "16", "memory": memory, "pods": "110"}
    n["status"] = {"capacity": res, "allocatable": res,
                   "conditions": [{"type": "Ready", "status": "True"}]}
    req("PUT", f"/api/v1/nodes/{name}/status", n)
    # A lease two hours ahead: the node lifecycle controller leaves it Ready.
    req("POST", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases", {
        "apiVersion": "coordination.k8s.io/v1", "kind": "Lease", "metadata": {"name": name},
        "spec": {"holderIdentity": name, "leaseDurationSeconds": 40, "renewTime": later}})
node("a", "3Gi"); node("b", "3Gi"); node("c", "1Gi")

NS = "/apis/kubevirt.io/v1/namespaces/default"
SUB = "/apis/subresources.kubevirt.io/v1/namespaces/default"
def vmi(name="vm"): return req("GET", f"{NS}/virtualmachineinstances/{name}")
def state(name="vm"): return vmi(name).get("status", {}).get("migrationState", {})
def migration(name): return call("GET", f"{NS}/virtualmachineinstancemigrations/{name}")
def phase(name): return migration(name)[1].get("status", {}).get("phase")
def kubelet(fields, name="vm"):  # the node's write, as rustkube-node#40 will make it
    code, out = merge(f"{NS}/virtualmachineinstances/{name}/status", {"status": {"migrationState": fields}})
    if code != 200: print(f"kubelet write {fields}: {code} {out}")
def migrate(path, opts={}):
    code, out = call("PUT", f"{SUB}/{path}/migrate", opts)
    name = out.get("message", "").split(" ")[1] if code == 200 else None
    return code, name, out
def now(): return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
def guest(name, size="2Gi"):
    req("POST", f"{NS}/virtualmachineinstances", {"apiVersion": "kubevirt.io/v1",
        "kind": "VirtualMachineInstance", "metadata": {"name": name},
        "spec": {"domain": {"cpu": {"cores": 1}, "memory": {"guest": size}, "devices": {}}}})

# --- discovery ----------------------------------------------------------------
res = [r["name"] for r in req("GET", "/apis/subresources.kubevirt.io/v1")["resources"]]
check("virtualmachines/migrate" in res and "virtualmachineinstances/migrate" in res,
      f"migrate advertised in subresources.kubevirt.io ({res})")

# --- a VM, placed and running -------------------------------------------------
req("POST", f"{NS}/virtualmachines", {"apiVersion": "kubevirt.io/v1", "kind": "VirtualMachine",
    "metadata": {"name": "vm"}, "spec": {"runStrategy": "Always", "template": {"spec": {
        "domain": {"cpu": {"cores": 1}, "memory": {"guest": "2Gi"}, "devices": {}}}}}})
src = until(lambda: call("GET", f"{NS}/virtualmachineinstances/vm")[0] == 200
            and vmi().get("status", {}).get("nodeName"), 60)
check(src in ("a", "b"), f"VMI placed on {src}")
other = "b" if src == "a" else "a"
code, _ = merge(f"{NS}/virtualmachineinstances/vm/status", {"status": {"phase": "Running"}})
check(code == 200, "VMI Running (kubelet)")

# --- migrate: scheduled, prepared, sent, moved --------------------------------
code, m1, out = migrate("virtualmachines/vm")
check(code == 200 and m1 and m1.startswith("kubevirt-migrate-vm-"), f"virtctl migrate: {code} {out.get('message')}")
st = until(lambda: state().get("targetNode") and state())
check(st and st.get("sourceNode") == src and st.get("targetNode") == other and st.get("mode") == "PreCopy",
      f"target chosen by the scheduler: {src} -> {st and st.get('targetNode')} (not the source, not 1Gi c)")
check(st and st.get("migrationUid") == migration(m1)[1]["metadata"]["uid"], "migrationUid is the migration's uid")
fin = migration(m1)[1]["metadata"].get("finalizers", [])
check("kubevirt.io/migrationJobFinalize" in fin, f"finalizer held ({fin})")
code, _, out = migrate("virtualmachineinstances/vm")
check(code == 409 and "already migrating" in out.get("message", ""), f"second migrate refused: {code}")
check(until(lambda: phase(m1) == "PreparingTarget"), f"PreparingTarget ({phase(m1)})")

# The target is charged while in flight: a second 2Gi guest fits nowhere.
guest("second")
time.sleep(3)
check(not vmi("second").get("status", {}).get("nodeName"),
      f"target capacity held: second guest unplaced ({vmi('second').get('status', {}).get('message', '')[:120]})")

kubelet({"targetNodeAddress": "192.0.2.10"})
check(until(lambda: phase(m1) == "TargetReady"), f"TargetReady ({phase(m1)})")
kubelet({"startTimestamp": now()})
check(until(lambda: phase(m1) == "Running"), f"Running ({phase(m1)})")
kubelet({"completed": True, "endTimestamp": now()})
check(until(lambda: phase(m1) == "Succeeded"), f"Succeeded ({phase(m1)})")
check(until(lambda: vmi().get("status", {}).get("nodeName") == other), f"VMI moved to {other}")
phases = [p["phase"] for p in migration(m1)[1]["status"].get("phaseTransitionTimestamps", [])]
check(phases == ["Pending", "Scheduling", "Scheduled", "PreparingTarget", "TargetReady", "Running", "Succeeded"],
      f"phase history {phases}")
mirror = migration(m1)[1]["status"].get("migrationState", {})
check(mirror.get("completed") is True and mirror.get("targetNode") == other, "migration mirrors the VMI's state")
check(until(lambda: vmi("second").get("status", {}).get("nodeName") == src),
      f"source freed: second guest placed on {src}")

# --- the source reports failure ----------------------------------------------
# a and b are each 2Gi used of 3Gi now: a fourth node takes the target.
node("d", "8Gi")
code, m2, _ = migrate("virtualmachineinstances/vm")
check(code == 200, f"second migration created ({m2})")
check(until(lambda: state().get("migrationUid") == migration(m2)[1]["metadata"]["uid"]
            and state().get("targetNode") == "d"), f"target d ({state().get('targetNode')})")
kubelet({"targetNodeAddress": "192.0.2.11"}); kubelet({"startTimestamp": now()})
until(lambda: phase(m2) == "Running")
kubelet({"failed": True, "failureReason": "e2e: transfer refused", "endTimestamp": now()})
check(until(lambda: phase(m2) == "Failed"), f"Failed ({phase(m2)})")
check(migration(m2)[1]["status"]["migrationState"].get("failureReason") == "e2e: transfer refused",
      "the source's reason is on the migration")
check(vmi().get("status", {}).get("nodeName") == other, "VMI stays where it was")

# --- delete before sending: abort ----------------------------------------------
code, m3, _ = migrate("virtualmachineinstances/vm")
check(until(lambda: phase(m3) == "PreparingTarget"), f"third migration preparing ({phase(m3)})")
call("DELETE", f"{NS}/virtualmachineinstancemigrations/{m3}")
check(until(lambda: migration(m3)[0] == 404), "deleted migration released (finalizer removed)")
st = state()
check(st.get("failed") is True and st.get("abortRequested") is True and st.get("abortStatus") == "Succeeded",
      f"abort recorded on the VMI ({st.get('failureReason')})")

# --- no node for the target ---------------------------------------------------
n = req("GET", "/api/v1/nodes/d"); n.setdefault("spec", {})["unschedulable"] = True; req("PUT", "/api/v1/nodes/d", n)
time.sleep(1)
code, m4, _ = migrate("virtualmachineinstances/vm")
def cond(name):
    return next((c for c in migration(name)[1].get("status", {}).get("conditions", [])
                 if c["type"] == "TargetScheduled"), {})
c = until(lambda: cond(m4).get("status") == "False" and cond(m4))
check(c and c.get("reason") == "Unschedulable", f"TargetScheduled=False: {c and c.get('message')}")
check(phase(m4) == "Scheduling", f"waits in Scheduling ({phase(m4)})")
n = req("GET", "/api/v1/nodes/d"); n.setdefault("spec", {})["unschedulable"] = False; req("PUT", "/api/v1/nodes/d", n)
check(until(lambda: state().get("targetNode") == "d"), "uncordoned: target placed")
check(until(lambda: cond(m4).get("status") == "True"), "TargetScheduled=True")
call("DELETE", f"{NS}/virtualmachineinstancemigrations/{m4}")
until(lambda: migration(m4)[0] == 404)

# --- addedNodeSelector (#208) -------------------------------------------------------
# d (8Gi) is the roomier target; e (4Gi, zone=x) is the second choice, which
# the selector forces.
node("e", "4Gi", {"zone": "x"})
code, m5, out = migrate("virtualmachineinstances/vm", {"addedNodeSelector": {"zone": "x"}})
check(code == 200 and migration(m5)[1]["spec"].get("addedNodeSelector") == {"zone": "x"},
      f"migrate with addedNodeSelector: {code}, carried on the migration ({out.get('message')})")
check(until(lambda: state().get("targetNode") == "e"), f"the selector picks e over the roomier d ({state().get('targetNode')})")
call("DELETE", f"{NS}/virtualmachineinstancemigrations/{m5}")
until(lambda: migration(m5)[0] == 404)
code, m6, _ = migrate("virtualmachineinstances/vm", {"addedNodeSelector": {"zone": "nowhere"}})
c = until(lambda: cond(m6).get("status") == "False" and cond(m6))
check(c and c.get("reason") == "Unschedulable", f"a selector no node meets: TargetScheduled=False ({c and c.get('message')})")
check(not state().get("targetNode") or state().get("migrationUid") != migration(m6)[1]["metadata"]["uid"],
      "and no target is set")
call("DELETE", f"{NS}/virtualmachineinstancemigrations/{m6}")
until(lambda: migration(m6)[0] == 404)
code, _, out = migrate("virtualmachineinstances/vm", {"addedNodeSelector": {"zone": 7}})
check(code == 422, f"a selector value that is not a string: 422 ({code})")

# --- a VMI that does not exist ---------------------------------------------------
gone = req("POST", f"{NS}/virtualmachineinstancemigrations", {"apiVersion": "kubevirt.io/v1",
    "kind": "VirtualMachineInstanceMigration", "metadata": {"name": "nothing"}, "spec": {"vmiName": "nope"}})
check(until(lambda: phase("nothing") == "Failed"), f"missing VMI: Failed ({phase('nothing')})")
check("does not exist" in migration("nothing")[1]["status"]["migrationState"].get("failureReason", ""),
      "and says why")

sys.exit(failed)
PY
report
