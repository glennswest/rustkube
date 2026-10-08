#!/usr/bin/env bash
#
# RuntimeClass (#135): node.k8s.io/v1 served, RuntimeClass admission, and the
# scheduler counting a Pod's overhead — a real apiserver and scheduler on
# fastetcd with one stand-in node (no kubelet).
#
# - /apis lists node.k8s.io; runtimeclasses: create, get, list, watch,
#   merge patch, update, protobuf GET, delete, deletecollection
# - a Pod naming a missing class: 403 "RuntimeClass \"x\" not found"
# - a class with overhead.podFixed and scheduling: the Pod gets spec.overhead,
#   the nodeSelector and the tolerations; a Pod with a different overhead,
#   or overhead without a class, is 403
# - the class deleted: a new Pod naming it is 403
# - scheduling: on a 1-CPU node a Pod asking 800m with a 250m-overhead class
#   stays Pending (Insufficient cpu); the same Pod without the class binds
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_scheduler
export API ADMIN
python3 - <<'PY' || FAIL=$?
import datetime, http.client, json, os, socket, ssl, sys, time, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, ctype="application/json", accept="application/json"):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype, "Accept": accept})
    r = c.getresponse(); raw = r.read(); ct = r.getheader("Content-Type", ""); c.close()
    try: return r.status, (json.loads(raw) if "json" in ct else raw)
    except ValueError: return r.status, raw
def until(f, secs=20):
    end = time.monotonic() + secs
    while time.monotonic() < end:
        v = f()
        if v: return v
        time.sleep(0.3)
    return f()
RC = "/apis/node.k8s.io/v1/runtimeclasses"
groups = [g["name"] for g in req("GET", "/apis")[1]["groups"]]
check("node.k8s.io" in groups, "/apis lists node.k8s.io")
res = req("GET", "/apis/node.k8s.io/v1")[1]["resources"]
check(any(r["name"] == "runtimeclasses" and r["namespaced"] is False for r in res), "discovery: runtimeclasses, cluster-scoped")
for i in (1, 2):
    code, _ = req("POST", RC, {"apiVersion": "node.k8s.io/v1", "kind": "RuntimeClass",
                               "metadata": {"name": f"e2e-{i}", "labels": {"e2e": "rc"}}, "handler": "runc"})
    if i == 1: check(code == 201, f"create ({code})")
code, out = req("GET", RC + "/e2e-1"); check(code == 200 and out["handler"] == "runc", f"get ({code})")
code, out = req("GET", RC + "?labelSelector=e2e%3Drc"); check(code == 200 and out["kind"] == "RuntimeClassList" and len(out["items"]) == 2, f"list ({code})")
c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=5)
c.request("GET", RC + "?watch=true&labelSelector=e2e%3Drc", headers={"Authorization": "Bearer " + os.environ["ADMIN"]})
line = c.getresponse().fp.readline(); c.close()
check(json.loads(line)["type"] == "ADDED", "watch")
code, out = req("PATCH", RC + "/e2e-1", {"metadata": {"annotations": {"p": "1"}}}, "application/merge-patch+json")
check(code == 200 and out["metadata"]["annotations"]["p"] == "1", f"merge patch ({code})")
out["metadata"]["labels"]["u"] = "1"
code, out = req("PUT", RC + "/e2e-1", out); check(code == 200 and out["metadata"]["labels"]["u"] == "1", f"update ({code})")
code, _ = req("GET", RC + "/e2e-1", accept="application/vnd.kubernetes.protobuf"); check(code == 200, f"protobuf GET ({code})")
code, _ = req("DELETE", RC + "/e2e-2"); check(code == 200, f"delete ({code})")
code, _ = req("DELETE", RC + "?labelSelector=e2e%3Drc")
check(code == 200 and req("GET", RC + "?labelSelector=e2e%3Drc")[1]["items"] == [], f"deletecollection ({code})")

P = "/api/v1/namespaces/default/pods"
def pod(name, rc=None, cpu="100m", **spec):
    s = dict({"containers": [{"name": "c", "image": "pause", "resources": {"requests": {"cpu": cpu}}}]}, **spec)
    if rc: s["runtimeClassName"] = rc
    return req("POST", P, {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name}, "spec": s})
code, out = pod("no-class", "nothere")
check(code == 403 and 'RuntimeClass "nothere" not found' in out.get("message", ""), f"missing class: 403 ({code} {out.get('message')})")
req("POST", RC, {"apiVersion": "node.k8s.io/v1", "kind": "RuntimeClass", "metadata": {"name": "kata"}, "handler": "kata",
                 "overhead": {"podFixed": {"cpu": "250m", "memory": "120Mi"}},
                 "scheduling": {"nodeSelector": {"sandbox": "true"}, "tolerations": [{"key": "sandbox", "operator": "Exists"}]}})
code, out = pod("with-overhead", "kata")
check(code == 201 and out["spec"].get("overhead") == {"cpu": "250m", "memory": "120Mi"}
      and out["spec"].get("nodeSelector", {}).get("sandbox") == "true"
      and {"key": "sandbox", "operator": "Exists"} in out["spec"].get("tolerations", []),
      f"overhead, nodeSelector and tolerations from the class ({code})")
code, out = pod("bad-overhead", "kata", overhead={"cpu": "1"})
check(code == 403 and "doesn't match RuntimeClass's defined Overhead" in out.get("message", ""), f"different overhead: 403 ({code})")
code, out = pod("stray-overhead", overhead={"cpu": "1"})
check(code == 403 and "without corresponding RuntimeClass" in out.get("message", ""), f"overhead without a class: 403 ({code})")
req("POST", RC, {"apiVersion": "node.k8s.io/v1", "kind": "RuntimeClass", "metadata": {"name": "gone"}, "handler": "runc"})
req("DELETE", RC + "/gone")
code, _ = pod("deleted-class", "gone")
check(code == 403, f"a deleted class: 403 ({code})")

# Scheduling counts overhead: one node, 1 CPU.
later = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=2)).strftime("%Y-%m-%dT%H:%M:%S.000000Z")
n = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node", "metadata": {"name": "n1",
        "labels": {"kubernetes.io/hostname": "n1", "sandbox": "true"}}})[1]
alloc = {"cpu": "1", "memory": "4Gi", "pods": "110"}
n["status"] = {"capacity": alloc, "allocatable": alloc, "conditions": [{"type": "Ready", "status": "True"}]}
req("PUT", "/api/v1/nodes/n1/status", n)
req("POST", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases", {"apiVersion": "coordination.k8s.io/v1",
    "kind": "Lease", "metadata": {"name": "n1"}, "spec": {"holderIdentity": "n1", "leaseDurationSeconds": 40, "renewTime": later}})
req("DELETE", P + "/with-overhead")
req("POST", RC, {"apiVersion": "node.k8s.io/v1", "kind": "RuntimeClass", "metadata": {"name": "heavy"}, "handler": "kata",
                 "overhead": {"podFixed": {"cpu": "250m"}}})
pod("big-heavy", "heavy", cpu="800m")
time.sleep(5)
cond = [c for c in req("GET", P + "/big-heavy")[1].get("status", {}).get("conditions", []) if c["type"] == "PodScheduled"]
check(not req("GET", P + "/big-heavy")[1]["spec"].get("nodeName") and cond and "Insufficient cpu" in cond[0].get("message", ""),
      f"800m + 250m overhead on a 1-CPU node: unschedulable ({cond})")
req("DELETE", P + "/big-heavy")
pod("big-plain", cpu="800m")
check(until(lambda: req("GET", P + "/big-plain")[1]["spec"].get("nodeName") == "n1"), "the same 800m without overhead binds")
sys.exit(failed)
PY
report
