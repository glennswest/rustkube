#!/usr/bin/env bash
#
# Scheduling gates (#87): a real apiserver and scheduler on fastetcd, one
# Ready node.
#
# - a Pod created with two gates is not bound, and reads
#   PodScheduled=False/SchedulingGated with no FailedScheduling Event
# - adding a gate to it is refused (422, upstream's message)
# - removing one gate leaves it waiting; removing the last binds it
# - an ungated Pod beside it is bound at once
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_scheduler
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, time, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, ctype="application/json"):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype})
    r = c.getresponse(); raw = r.read(); c.close()
    return r.status, json.loads(raw) if raw else None
def until(f, secs=20):
    end = time.time() + secs
    while time.time() < end:
        v = f()
        if v: return v
        time.sleep(0.5)
    return None

_, n = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node", "metadata": {"name": "n1"}})
res = {"cpu": "8", "memory": "16Gi", "pods": "110"}
n["status"] = {"capacity": res, "allocatable": res, "conditions": [{"type": "Ready", "status": "True"}]}
req("PUT", "/api/v1/nodes/n1/status", n)
req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "gates"}})
P = "/api/v1/namespaces/gates/pods"
def pod(name, gates):
    body = {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name},
            "spec": {"containers": [{"name": "c", "image": "unused"}]}}
    if gates: body["spec"]["schedulingGates"] = [{"name": g} for g in gates]
    return req("POST", P, body)
node_of = lambda name: req("GET", f"{P}/{name}")[1]["spec"].get("nodeName")

pod("gated", ["example.com/arch", "example.com/quota"])
pod("free", [])
check(until(lambda: node_of("free")) == "n1", "an ungated Pod is bound")
def gated_cond():
    c = [c for c in req("GET", f"{P}/gated")[1].get("status", {}).get("conditions", []) if c["type"] == "PodScheduled"]
    return c[0] if c and c[0].get("reason") == "SchedulingGated" else None
c = until(gated_cond)
check(c is not None and c["status"] == "False", f"the gated Pod reads PodScheduled=False/SchedulingGated ({c})")
check(node_of("gated") is None, "…and is not bound")
_, ev = req("GET", "/api/v1/namespaces/gates/events")
check(not [e for e in ev["items"] if e.get("reason") == "FailedScheduling" and e["involvedObject"]["name"] == "gated"],
      "no FailedScheduling Event for a gated Pod")

code, out = req("PATCH", f"{P}/gated", {"spec": {"schedulingGates": [{"name": "example.com/arch"}, {"name": "example.com/quota"}, {"name": "x"}]}},
                "application/merge-patch+json")
check(code == 422 and "only deletion is allowed, but found new scheduling gate 'x'" in json.dumps(out), f"adding a gate is refused ({code})")

req("PATCH", f"{P}/gated", {"spec": {"schedulingGates": [{"name": "example.com/quota"}]}}, "application/merge-patch+json")
time.sleep(3)
check(node_of("gated") is None, "one gate left: still waiting")
req("PATCH", f"{P}/gated", {"spec": {"schedulingGates": None}}, "application/merge-patch+json")
check(until(lambda: node_of("gated")) == "n1", "the last gate removed: bound")
sys.exit(failed)
PY
report
