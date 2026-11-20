#!/usr/bin/env bash
#
# Resource fit beyond cpu and memory (#297), against a real apiserver,
# scheduler and controller-manager on fastetcd with one stand-in node (no
# kubelet: a Pod's binding is the evidence, its deletion immediate).
#
# The node advertises `example.com/foo: 2` and `ephemeral-storage: 10Gi`.
# - two Pods of 1 example.com/foo bind; a third waits, its PodScheduled
#   message naming `Insufficient example.com/foo`
# - a Pod asking for `nvidia.com/gpu`, which the node does not advertise,
#   waits: `Insufficient nvidia.com/gpu`
# - a Pod asking for 20Gi of ephemeral-storage waits: `Insufficient
#   ephemeral-storage`; one asking for 1Gi binds
# - with priorities: a high-priority Pod of 1 example.com/foo preempts
#   exactly one of the two low Pods holding it and binds (conformance's
#   SchedulerPreemption specs size their Pods this way)
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
alloc = {"cpu": "4", "memory": "8Gi", "pods": "110", "ephemeral-storage": "10Gi", "example.com/foo": "2"}
n["status"] = {"capacity": alloc, "allocatable": alloc, "conditions": [{"type": "Ready", "status": "True"}]}
req("PUT", "/api/v1/nodes/n1/status", n)
req("POST", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases", {"apiVersion": "coordination.k8s.io/v1",
    "kind": "Lease", "metadata": {"name": "n1"}, "spec": {"holderIdentity": "n1", "leaseDurationSeconds": 40, "renewTime": later}})
for name, value in [("low", 100), ("high", 1000)]:
    req("POST", "/apis/scheduling.k8s.io/v1/priorityclasses", {"apiVersion": "scheduling.k8s.io/v1", "kind": "PriorityClass",
        "metadata": {"name": name}, "value": value})
NS = "/api/v1/namespaces/default"
def pod(name, cls, requests):
    return req("POST", NS + "/pods", {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "labels": {"app": name}},
        "spec": {"priorityClassName": cls, "containers": [{"name": "c", "image": "pause",
                 "resources": {"requests": dict({"cpu": "100m"}, **requests)}}]}})
get = lambda name: req("GET", NS + f"/pods/{name}")
bound = lambda name: (lambda c, o: c == 200 and o["spec"].get("nodeName") == "n1")(*get(name))
gone = lambda name: get(name)[0] == 404
def why(name):
    conds = get(name)[1].get("status", {}).get("conditions", [])
    return next((c.get("message", "") for c in conds if c["type"] == "PodScheduled" and c["status"] == "False"), "")

for p in ("foo-1", "foo-2"):
    pod(p, "low", {"example.com/foo": "1"})
check(until(lambda: bound("foo-1") and bound("foo-2")), "two Pods of 1 example.com/foo bind on a node with 2")
pod("foo-3", "low", {"example.com/foo": "1"})
m = until(lambda: why("foo-3"))
check("Insufficient example.com/foo" in m and not bound("foo-3"), f"the third waits: {m!r}")

pod("gpu", "low", {"nvidia.com/gpu": "1"})
m = until(lambda: why("gpu"))
check("Insufficient nvidia.com/gpu" in m and not bound("gpu"), f"a resource the node does not advertise: {m!r}")

pod("disk-big", "low", {"ephemeral-storage": "20Gi"})
m = until(lambda: why("disk-big"))
check("Insufficient ephemeral-storage" in m and not bound("disk-big"), f"20Gi of ephemeral-storage on a 10Gi node: {m!r}")
pod("disk-small", "low", {"ephemeral-storage": "1Gi"})
check(until(lambda: bound("disk-small")), "1Gi of ephemeral-storage binds")

# Out of the way, so only the preemptor wants the room it makes.
for p in ("foo-3", "gpu", "disk-big"):
    req("DELETE", NS + f"/pods/{p}")
until(lambda: all(gone(p) for p in ("foo-3", "gpu", "disk-big")))
pod("foo-high", "high", {"example.com/foo": "1"})
check(until(lambda: gone("foo-1") != gone("foo-2")), "the high-priority Pod preempts exactly one low Pod holding the resource")
check(until(lambda: bound("foo-high")), "the high-priority Pod binds")
check(bound("disk-small"), "a Pod not holding the resource is not a victim")
sys.exit(failed)
PY
report
