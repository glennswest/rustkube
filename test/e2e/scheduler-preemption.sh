#!/usr/bin/env bash
#
# Scheduler preemption (#84), against a real apiserver, scheduler and
# controller-manager on fastetcd with one stand-in node of 1 CPU (no
# kubelet: a Pod's deletion is immediate, its binding is the evidence).
#
# - two low-priority Pods of 400m fill the node; a high-priority Pod of 600m
#   fits nowhere, is nominated to the node (status.nominatedNodeName),
#   exactly one low Pod is evicted (a "Preempted" Event on it), and the
#   high Pod binds; nominatedNodeName is gone once bound
# - preemptionPolicy: Never and an equal-priority Pod preempt nothing
# - with a PodDisruptionBudget protecting the low Pod (disruptionsAllowed 0),
#   a top-priority Pod preempts the unprotected high Pod instead and binds;
#   the protected one stays
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
start_scheduler
export API ADMIN
python3 - <<'PY' || FAIL=$?
import datetime, http.client, json, os, ssl, sys, time, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": "application/json"})
    r = c.getresponse(); raw = r.read(); c.close()
    try: return r.status, json.loads(raw)
    except ValueError: return r.status, raw
def until(f, secs=30):
    end = time.monotonic() + secs
    while time.monotonic() < end:
        v = f()
        if v: return v
        time.sleep(0.3)
    return f()
later = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=2)).strftime("%Y-%m-%dT%H:%M:%S.000000Z")
n = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node", "metadata": {"name": "n1", "labels": {"kubernetes.io/hostname": "n1"}}})[1]
alloc = {"cpu": "1", "memory": "4Gi", "pods": "110"}
n["status"] = {"capacity": alloc, "allocatable": alloc, "conditions": [{"type": "Ready", "status": "True"}]}
req("PUT", "/api/v1/nodes/n1/status", n)
req("POST", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases", {"apiVersion": "coordination.k8s.io/v1",
    "kind": "Lease", "metadata": {"name": "n1"}, "spec": {"holderIdentity": "n1", "leaseDurationSeconds": 40, "renewTime": later}})
for name, value in [("low", 100), ("high", 1000)]:
    req("POST", "/apis/scheduling.k8s.io/v1/priorityclasses", {"apiVersion": "scheduling.k8s.io/v1", "kind": "PriorityClass",
        "metadata": {"name": name}, "value": value})
NS = "/api/v1/namespaces/default"
def pod(name, cls, cpu, labels=None, **spec):
    return req("POST", NS + "/pods", {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "labels": labels or {"app": name}},
        "spec": dict({"priorityClassName": cls, "containers": [{"name": "c", "image": "pause", "resources": {"requests": {"cpu": cpu}}}]}, **spec)})[1]
get = lambda name: req("GET", NS + f"/pods/{name}")
bound = lambda name: (lambda c, o: c == 200 and o["spec"].get("nodeName") == "n1")(*get(name))
gone = lambda name: get(name)[0] == 404

for i in (1, 2):
    pod(f"low-{i}", "low", "400m", {"tier": "low"})
check(until(lambda: bound("low-1") and bound("low-2")), "two low-priority 400m Pods bind")
pod("high-1", "high", "600m")
check(until(lambda: gone("low-1") != gone("low-2")), "exactly one low Pod is evicted")
check(until(lambda: bound("high-1")), "the high-priority Pod binds")
victim = "low-1" if gone("low-1") else "low-2"
check("nominatedNodeName" not in get("high-1")[1].get("status", {}), "nominatedNodeName gone once bound")
evs = req("GET", NS + "/events")[1]["items"]
check(any(e.get("reason") == "Preempted" and e["involvedObject"]["name"] == victim for e in evs), f"a Preempted Event on {victim}")
survivor = "low-2" if victim == "low-1" else "low-1"

pod("never", "high", "600m", preemptionPolicy="Never")
pod("peer", "low", "600m")
time.sleep(5)
check(not bound("never") and not bound("peer") and bound(survivor) and bound("high-1"),
      "preemptionPolicy Never and an equal-priority Pod preempt nothing")
req("DELETE", NS + "/pods/never"); req("DELETE", NS + "/pods/peer")

req("POST", "/apis/policy/v1/namespaces/default/poddisruptionbudgets", {"apiVersion": "policy/v1", "kind": "PodDisruptionBudget",
    "metadata": {"name": "keep-low"}, "spec": {"minAvailable": 1, "selector": {"matchLabels": {"tier": "low"}}}})
until(lambda: req("GET", "/apis/policy/v1/namespaces/default/poddisruptionbudgets/keep-low")[1].get("status", {}).get("observedGeneration") is not None)
req("POST", "/apis/scheduling.k8s.io/v1/priorityclasses", {"apiVersion": "scheduling.k8s.io/v1", "kind": "PriorityClass",
    "metadata": {"name": "top"}, "value": 2000})
pod("top-1", "top", "500m")
time.sleep(6)
cond = [c for c in get("top-1")[1].get("status", {}).get("conditions", []) if c["type"] == "PodScheduled"]
# Freeing 500m needs the high Pod (600m, unprotected) or the low one (protected):
# the budget-protected low Pod is spared and the high Pod is the victim.
check(bound(survivor), f"the PDB-protected low Pod is not evicted ({cond})")
check(until(lambda: bound("top-1")), "top-1 binds after preempting the unprotected Pod")
sys.exit(failed)
PY
report
