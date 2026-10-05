#!/usr/bin/env bash
#
# VMI launcher Pods (#203) on a real apiserver, controller-manager and
# scheduler over a real fastetcd. There is no kubelet: the rig plays the
# parts rustkube-node#88 plays — writes the launcher Pod's status (phase,
# podIP, Ready), the VMI's migrationState as rustkube-node#40 does, and
# confirms a terminating launcher Pod's deletion.
#
#   test/e2e/vmi-launcher.sh       # from the checkout; builds what it runs
#
# - a VM on the pod network gets one launcher Pod once its VMI is placed:
#   virt-launcher-<vmi>-<5>, KubeVirt's labels, owned by the VMI, on the
#   VMI's node, never scheduled separately, no status from the controller
# - a VMI on a host bridge (storm.io/bridge) gets none
# - two VMs in one namespace each get their own
# - the kubelet's status on it makes it a Service endpoint
# - a launcher deleted out from under its VMI is replaced
# - a live migration gives the target its own (migrationJobUID); success
#   deletes the source's, failure the target's
# - stopping the VM takes the launcher with the VMI
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
start_scheduler
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, re, ssl, sys, time, urllib.parse, datetime

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
def now(): return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

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
def node(name):
    n = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node",
        "metadata": {"name": name, "labels": {"kubernetes.io/hostname": name}}})
    res = {"cpu": "16", "memory": "8Gi", "pods": "110"}
    n["status"] = {"capacity": res, "allocatable": res,
                   "conditions": [{"type": "Ready", "status": "True"}]}
    req("PUT", f"/api/v1/nodes/{name}/status", n)
    req("POST", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases", {
        "apiVersion": "coordination.k8s.io/v1", "kind": "Lease", "metadata": {"name": name},
        "spec": {"holderIdentity": name, "leaseDurationSeconds": 40, "renewTime": later}})
node("a"); node("b")

NS = "/apis/kubevirt.io/v1/namespaces/default"
SUB = "/apis/subresources.kubevirt.io/v1/namespaces/default"
def vmi(name): return call("GET", f"{NS}/virtualmachineinstances/{name}")[1]
def vm(name, annotations=None, labels=None):
    req("POST", f"{NS}/virtualmachines", {"apiVersion": "kubevirt.io/v1", "kind": "VirtualMachine",
        "metadata": {"name": name}, "spec": {"runStrategy": "Always", "template": {
            "metadata": {"labels": labels or {}, "annotations": annotations or {}},
            "spec": {"domain": {"cpu": {"cores": 1}, "memory": {"guest": "1Gi"},
                                "devices": {"interfaces": [{"name": "default", "bridge": {}}]}},
                     "networks": [{"name": "default", "pod": {}}]}}}})
def launchers(name, live_only=True):
    uid = vmi(name).get("metadata", {}).get("uid", "-")
    pods = req("GET", "/api/v1/namespaces/default/pods?labelSelector="
               + urllib.parse.quote(f"kubevirt.io/created-by={uid}"))["items"]
    return [p for p in pods if not live_only or not p["metadata"].get("deletionTimestamp")]
def kubelet_pod(p, phase="Running", ip=None):  # the node's write, as rustkube-node#88 makes it
    p = req("GET", f"/api/v1/namespaces/default/pods/{p['metadata']['name']}")
    ready = "True" if phase == "Running" else "False"
    p["status"] = {"phase": phase, "conditions": [{"type": t, "status": s, "lastTransitionTime": now()}
                   for t, s in [("PodScheduled", "True"), ("Initialized", "True"),
                                ("ContainersReady", ready), ("Ready", ready)]], "startTime": now()}
    if ip: p["status"].update({"podIP": ip, "podIPs": [{"ip": ip}]})
    code, out = call("PUT", f"/api/v1/namespaces/default/pods/{p['metadata']['name']}/status", p)
    if code != 200: print(f"kubelet status write: {code} {out}")
def confirm_deletions():  # the kubelet lets a terminating launcher go
    for p in req("GET", "/api/v1/namespaces/default/pods?labelSelector=kubevirt.io%3Dvirt-launcher")["items"]:
        if p["metadata"].get("deletionTimestamp"):
            call("DELETE", f"/api/v1/namespaces/default/pods/{p['metadata']['name']}",
                 {"apiVersion": "v1", "kind": "DeleteOptions", "gracePeriodSeconds": 0,
                  "preconditions": {"uid": p["metadata"]["uid"]}})
def gone_or_going(name):
    code, p = call("GET", f"/api/v1/namespaces/default/pods/{name}")
    return code == 404 or bool(p.get("metadata", {}).get("deletionTimestamp"))

# --- a VM on the pod network --------------------------------------------------
vm("web", labels={"app": "web"})
node_of = until(lambda: vmi("web").get("status", {}).get("nodeName"), 60)
check(node_of in ("a", "b"), f"VMI placed on {node_of}")
ps = until(lambda: launchers("web"))
check(len(ps or []) == 1, f"one launcher Pod ({len(ps or [])})")
p = (ps or [{}])[0]; m = p.get("metadata", {}); l = m.get("labels", {})
uid = vmi("web")["metadata"]["uid"]
check(re.fullmatch(r"virt-launcher-web-[a-z0-9]{5}", m.get("name", "")) is not None, f"name {m.get('name')}")
check(l.get("kubevirt.io") == "virt-launcher" and l.get("kubevirt.io/created-by") == uid
      and l.get("vm.kubevirt.io/name") == "web" and l.get("app") == "web",
      f"KubeVirt's labels and the VMI's own ({l})")
o = (m.get("ownerReferences") or [{}])[0]
check(o.get("kind") == "VirtualMachineInstance" and o.get("uid") == uid and o.get("controller") is True
      and o.get("blockOwnerDeletion") is True, f"owned by the VMI ({o})")
check(p.get("spec", {}).get("nodeName") == node_of, f"on the VMI's node ({p.get('spec', {}).get('nodeName')})")
check([c["name"] for c in p.get("spec", {}).get("containers", [])] == ["compute"], "one container, compute")
time.sleep(3)
again = launchers("web")
check(len(again) == 1 and again[0]["metadata"]["uid"] == m.get("uid"), "still the one, a few seconds later")
check(again[0].get("status", {}).get("phase") in (None, "Pending"), f"no status but the default ({again[0].get('status')})")

# --- the kubelet's status makes it an endpoint ---------------------------------
req("POST", "/api/v1/namespaces/default/services", {"apiVersion": "v1", "kind": "Service",
    "metadata": {"name": "web"}, "spec": {"selector": {"app": "web"}, "ports": [{"port": 80}]}})
kubelet_pod(again[0], ip="10.0.1.5")
def endpoint_ips():
    sl = req("GET", "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices?labelSelector="
             + urllib.parse.quote("kubernetes.io/service-name=web"))["items"]
    return sorted(a for s in sl for e in s.get("endpoints") or [] for a in e.get("addresses", [])
                  if (e.get("conditions") or {}).get("ready") is not False)
check(until(lambda: endpoint_ips() == ["10.0.1.5"]), f"Service endpoint is the VM's pod IP ({endpoint_ips()})")

# --- a host bridge needs none; two VMs each have their own ---------------------
vm("bridged", annotations={"storm.io/bridge": "stormbr0"})
until(lambda: vmi("bridged").get("status", {}).get("nodeName"), 60)
time.sleep(3)
check(launchers("bridged") == [], "storm.io/bridge VMI: no launcher Pod")
vm("db")
until(lambda: vmi("db").get("status", {}).get("nodeName"), 60)
dbp = until(lambda: launchers("db"))
check(len(dbp or []) == 1 and dbp[0]["metadata"]["name"].startswith("virt-launcher-db-"),
      "a second VM in the namespace gets its own")

# --- deleted from under its VMI: replaced ------------------------------------
old = launchers("db")[0]["metadata"]["name"]
call("DELETE", f"/api/v1/namespaces/default/pods/{old}", {"apiVersion": "v1", "kind": "DeleteOptions",
     "gracePeriodSeconds": 0})
confirm_deletions()
new = until(lambda: [p for p in launchers("db") if p["metadata"]["name"] != old])
check(bool(new) and len(launchers("db")) == 1, f"replaced: {old} -> {new and new[0]['metadata']['name']}")

# --- migration: the target gets one, the loser's goes ---------------------------
merge(f"{NS}/virtualmachineinstances/web/status", {"status": {"phase": "Running"}})
code, out = call("PUT", f"{SUB}/virtualmachineinstances/web/migrate", {})
check(code == 200, f"migrate: {code} {out.get('message')}")
st = lambda: vmi("web").get("status", {}).get("migrationState", {})
target = until(lambda: st().get("targetNode"))
check(target and target != node_of, f"target {target}")
both = until(lambda: len(launchers("web")) == 2 and launchers("web"))
tp = [p for p in both or [] if p["spec"]["nodeName"] == target]
check(len(tp) == 1 and tp[0]["metadata"]["labels"].get("kubevirt.io/migrationJobUID") == st().get("migrationUid"),
      f"target Pod on {target}, labelled with the migration")
src_pod = [p for p in both or [] if p["spec"]["nodeName"] == node_of]
def kubelet_vmi(fields):
    merge(f"{NS}/virtualmachineinstances/web/status", {"status": {"migrationState": fields}})
kubelet_vmi({"targetNodeAddress": "192.0.2.10"}); kubelet_vmi({"startTimestamp": now()})
kubelet_vmi({"completed": True, "endTimestamp": now()})
check(until(lambda: vmi("web").get("status", {}).get("nodeName") == target), "VMI moved to the target")
check(src_pod and until(lambda: gone_or_going(src_pod[0]["metadata"]["name"])),
      "the source's launcher is deleted")
confirm_deletions()
left = until(lambda: len(launchers("web", live_only=False)) == 1 and launchers("web", live_only=False))
check(bool(left) and left[0]["spec"]["nodeName"] == target, "one launcher left, on the target")

# A failed migration: the target's goes, the VMI's stays.
code, out = call("PUT", f"{SUB}/virtualmachineinstances/web/migrate", {})
t2 = until(lambda: st().get("migrationUid") and not st().get("completed") and st().get("targetNode"))
two = until(lambda: len(launchers("web")) == 2 and launchers("web"))
check(bool(two), f"second migration to {t2}: a target Pod")
tpod = [p for p in two or [] if p["spec"]["nodeName"] == t2]
kubelet_vmi({"failed": True, "failureReason": "e2e: refused", "endTimestamp": now()})
check(tpod and until(lambda: gone_or_going(tpod[0]["metadata"]["name"])), "failed: the target's is deleted")
confirm_deletions()
check(until(lambda: [p["spec"]["nodeName"] for p in launchers("web", live_only=False)] == [target]),
      "the VMI's own launcher stays")

# --- stopping the VM takes the launcher with the VMI -----------------------------
db_uid = vmi("db")["metadata"]["uid"]
code, _ = call("PUT", f"{SUB}/virtualmachines/db/stop", {})
check(code in (200, 202), f"virtctl stop: {code}")
def db_launchers():
    return req("GET", "/api/v1/namespaces/default/pods?labelSelector="
               + urllib.parse.quote(f"kubevirt.io/created-by={db_uid}"))["items"]
def stopped():
    confirm_deletions()
    return call("GET", f"{NS}/virtualmachineinstances/db")[0] == 404 and db_launchers() == []
check(until(stopped, 60), f"VMI and its launcher gone ({[p['metadata']['name'] for p in db_launchers()]})")

sys.exit(failed)
PY
report
