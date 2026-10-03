#!/usr/bin/env bash
#
# The scheduler holds a node to its allocatable.pods (#194): pvetest1 had
# 1,000 BestEffort Pods bound to a node that allows 110. A real apiserver and
# scheduler, stand-in Nodes, no kubelet and no controller-manager.
#
#   - a 2-pod node and 3 BestEffort Pods: two bind, the third stays unbound
#     with PodScheduled=False/Unschedulable "0/1 nodes are available: 1 Too
#     many pods.", and is still unbound after the scheduler has had time to
#     try again
#   - one bound Pod goes Succeeded: the third binds, PodScheduled=True
#   - a burst of 30 onto a 5-pod node (binds in flight, reservations): exactly
#     5 bound; deleting one bound Pod lets exactly one more bind
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_scheduler
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"]); TOKEN = os.environ["ADMIN"]
CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
def req(method, path, body=None, ctype="application/json"):
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + TOKEN, "Content-Type": ctype})
    r = conn.getresponse(); data = r.read()
    if r.status >= 300 and r.status != 409:
        print(f"setup {method} {path}: {r.status} {data[:300]!r}"); sys.exit(100)
    return json.loads(data)

def node(name, pods):
    n = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node",
        "metadata": {"name": name, "labels": {"kubernetes.io/hostname": name}}})
    res = {"cpu": "64", "memory": "256Gi", "pods": str(pods)}
    n["status"] = {"capacity": res, "allocatable": res,
                   "conditions": [{"type": "Ready", "status": "True"}]}
    req("PUT", f"/api/v1/nodes/{name}/status", n)

def namespace(ns, on):
    req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": ns}})
    return lambda name: req("POST", f"/api/v1/namespaces/{ns}/pods", {
        "apiVersion": "v1", "kind": "Pod", "metadata": {"name": name},
        "spec": {"nodeSelector": {"kubernetes.io/hostname": on},
                 "containers": [{"name": "c", "image": "unused"}]}})

def pods(ns):
    return {p["metadata"]["name"]: p for p in req("GET", f"/api/v1/namespaces/{ns}/pods")["items"]}
def bound(ns):
    return sorted(n for n, p in pods(ns).items()
                  if p["spec"].get("nodeName") and p.get("status", {}).get("phase") not in ("Succeeded", "Failed"))
def scheduled(p):
    return next((c for c in p.get("status", {}).get("conditions", []) if c["type"] == "PodScheduled"), {})
def until(f, limit=30):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        v = f()
        if v: return v
        time.sleep(0.2)
    return f()

# --- 2-pod node, 3 Pods --------------------------------------------------------
node("small", 2)
create = namespace("limit", "small")
for name in ("a", "b", "c"):
    create(name)
until(lambda: len(bound("limit")) >= 2)
want = "0/1 nodes are available: 1 Too many pods."
def refused():
    for n, p in pods("limit").items():
        c = scheduled(p)
        if not p["spec"].get("nodeName") and c.get("status") == "False":
            return n, c
first = until(refused)
check(len(bound("limit")) == 2, f"2-pod node: 2 of 3 bound ({bound('limit')})")
check(bool(first) and first[1].get("reason") == "Unschedulable" and first[1].get("message") == want,
      f"third Pod PodScheduled=False/Unschedulable {want!r} (got {first})")
time.sleep(5)
check(len(bound("limit")) == 2, f"still 2 bound after 5 s ({bound('limit')})")
# Re-tries do not rewrite an unchanged condition.
rv = pods("limit")[first[0]]["metadata"]["resourceVersion"] if first else None
time.sleep(3)
check(first is not None and pods("limit")[first[0]]["metadata"]["resourceVersion"] == rv,
      "an unchanged Unschedulable condition is not rewritten")

# --- one finishes: the third binds ---------------------------------------------
done = bound("limit")[0]
req("PATCH", f"/api/v1/namespaces/limit/pods/{done}/status",
    {"status": {"phase": "Succeeded"}}, "application/merge-patch+json")
third = first[0] if first else "c"
p = until(lambda: (lambda p: p if p["spec"].get("nodeName") else None)(pods("limit")[third]))
check(bool(p) and p["spec"]["nodeName"] == "small", f"{third} binds once {done} Succeeded")
check(bool(p) and scheduled(p).get("status") == "True", f"{third} PodScheduled=True ({scheduled(p) if p else None})")
check(len(bound("limit")) == 2, f"2 non-terminal bound ({bound('limit')})")

# --- burst of 30 onto a 5-pod node -----------------------------------------------
node("five", 5)
create = namespace("burst", "five")
for i in range(30):
    create(f"p{i:02}")
until(lambda: len(bound("burst")) >= 5)
time.sleep(5)
check(len(bound("burst")) == 5, f"burst of 30 on a 5-pod node: {len(bound('burst'))} bound")
reported = until(lambda: all(scheduled(p).get("message") == want
                            for p in pods("burst").values() if not p["spec"].get("nodeName")))
check(reported, "all 25 unbound report Too many pods")
gone = bound("burst")[0]
req("DELETE", f"/api/v1/namespaces/burst/pods/{gone}?gracePeriodSeconds=0")
until(lambda: gone not in pods("burst") and len(bound("burst")) >= 5)
time.sleep(5)
check(len(bound("burst")) == 5, f"after deleting {gone}: {len(bound('burst'))} bound (one replaced it)")
sys.exit(failed)
PY
report
