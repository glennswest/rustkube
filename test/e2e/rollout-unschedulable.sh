#!/usr/bin/env bash
#
# A rolling update whose surge pod cannot schedule until the old pod is gone
# (#266, stormcos#482): cilium-operator's shape on one node — replicas 1,
# maxSurge 1, maxUnavailable 1, a required pod anti-affinity on
# kubernetes.io/hostname. A real apiserver, controller-manager and
# scheduler; one stand-in Node kept alive by its Lease; this script plays the
# kubelet (marks bound pods Ready, finishes their deletion).
#
#   1. the Deployment rolls out its first pod, Ready
#   2. a template change: the new pod is Unschedulable (anti-affinity), and
#      the old ReplicaSet is scaled to 0 anyway, as upstream's
#      reconcileOldReplicaSets does with maxUnavailable 1
#   3. the old pod goes, the new one binds, is Ready: the rollout completes
#   4. control: the same with maxUnavailable 0 keeps the old pod (the floor is
#      1), as upstream — that Deployment cannot roll on one node
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
start_scheduler
export API ADMIN
python3 - <<'PY' || FAIL=$?
import datetime, http.client, json, os, ssl, sys, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"]); TOKEN = os.environ["ADMIN"]
CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(method, path, body=None, ctype="application/json", ok404=False):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + TOKEN, "Content-Type": ctype})
    r = conn.getresponse(); data = r.read()
    if r.status == 404 and ok404:
        return None
    if r.status >= 300 and r.status != 409:
        print(f"setup {method} {path}: {r.status} {data[:300]!r}"); sys.exit(100)
    return json.loads(data)

def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")

NODE = "n1"
req("POST", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases", {"apiVersion": "coordination.k8s.io/v1",
    "kind": "Lease", "metadata": {"name": NODE}, "spec": {"holderIdentity": NODE, "leaseDurationSeconds": 40, "renewTime": now()}})
n = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node",
    "metadata": {"name": NODE, "labels": {"kubernetes.io/hostname": NODE}}})
res = {"cpu": "64", "memory": "256Gi", "pods": "110"}
n["status"] = {"capacity": res, "allocatable": res, "conditions": [{"type": "Ready", "status": "True"}]}
req("PUT", f"/api/v1/nodes/{NODE}/status", n)
last_renew = time.monotonic()

def kubelet(ns):
    """One pass of the stand-in kubelet: renew the Lease; mark bound pods
    Running and Ready; finish deleting a terminating pod."""
    global last_renew
    if time.monotonic() - last_renew > 5:
        l = req("GET", f"/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/{NODE}")
        l["spec"]["renewTime"] = now()
        req("PUT", f"/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/{NODE}", l)
        last_renew = time.monotonic()
    for p in req("GET", f"/api/v1/namespaces/{ns}/pods")["items"]:
        name = p["metadata"]["name"]
        if p["metadata"].get("deletionTimestamp"):
            req("DELETE", f"/api/v1/namespaces/{ns}/pods/{name}?gracePeriodSeconds=0", ok404=True)
            continue
        if p["spec"].get("nodeName") and p.get("status", {}).get("phase") != "Running":
            p["status"] = {"phase": "Running", "conditions": [
                {"type": "PodScheduled", "status": "True"}, {"type": "Ready", "status": "True"}]}
            req("PUT", f"/api/v1/namespaces/{ns}/pods/{name}/status", p)

def until(ns, f, limit=60):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        kubelet(ns)
        v = f()
        if v: return v
        time.sleep(0.3)
    return f()

def deployment(ns, unavailable):
    req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": ns}})
    req("POST", f"/apis/apps/v1/namespaces/{ns}/deployments", {"apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "op"}, "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "op"}},
        "strategy": {"type": "RollingUpdate", "rollingUpdate": {"maxSurge": 1, "maxUnavailable": unavailable}},
        "template": {"metadata": {"labels": {"app": "op"}}, "spec": {
            "nodeSelector": {"kubernetes.io/hostname": NODE},
            "affinity": {"podAntiAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [
                {"labelSelector": {"matchLabels": {"app": "op"}}, "topologyKey": "kubernetes.io/hostname"}]}},
            "containers": [{"name": "c", "image": "unused"}]}}}})

def dep(ns):
    return req("GET", f"/apis/apps/v1/namespaces/{ns}/deployments/op")
def rss(ns):
    return req("GET", f"/apis/apps/v1/namespaces/{ns}/replicasets")["items"]
def rs_of(ns, template_marker):
    for r in rss(ns):
        if r["spec"]["template"]["metadata"].get("annotations", {}).get("rev") == template_marker:
            return r
def change_template(ns):
    d = dep(ns)
    d["spec"]["template"]["metadata"]["annotations"] = {"rev": "2"}
    req("PUT", f"/apis/apps/v1/namespaces/{ns}/deployments/op", d)
def unschedulable(ns):
    for p in req("GET", f"/api/v1/namespaces/{ns}/pods")["items"]:
        c = next((c for c in p.get("status", {}).get("conditions", []) if c["type"] == "PodScheduled"), {})
        if c.get("status") == "False" and "nti" in c.get("message", ""):
            return c["message"]

# --- 1-3: maxUnavailable 1 ----------------------------------------------------
ns = "roll"
deployment(ns, 1)
check(until(ns, lambda: dep(ns).get("status", {}).get("availableReplicas") == 1), "first rollout: 1 available")
change_template(ns)
msg = until(ns, lambda: unschedulable(ns), 30)
check(bool(msg), f"the surge pod is unschedulable beside the old one: {msg!r}")
old = lambda: next((r for r in rss(ns) if r["spec"]["template"]["metadata"].get("annotations", {}).get("rev") != "2"), None)
check(until(ns, lambda: old() and old()["spec"]["replicas"] == 0, 30), "old ReplicaSet scaled to 0 under maxUnavailable 1")
def done():
    s = dep(ns).get("status", {})
    new = rs_of(ns, "2")
    return (s.get("updatedReplicas") == 1 and s.get("availableReplicas") == 1 and s.get("replicas") == 1
            and new and new.get("status", {}).get("availableReplicas") == 1)
check(until(ns, done, 60), f"rollout completes: {dep(ns).get('status')}")
pods = req("GET", f"/api/v1/namespaces/{ns}/pods")["items"]
check(len(pods) == 1 and pods[0]["spec"].get("nodeName") == NODE, f"one pod, on {NODE}: {[p['metadata']['name'] for p in pods]}")

# --- 4: control, maxUnavailable 0 ---------------------------------------------
ns = "hold"
deployment(ns, 0)
check(until(ns, lambda: dep(ns).get("status", {}).get("availableReplicas") == 1), "control: first rollout")
change_template(ns)
check(bool(until(ns, lambda: unschedulable(ns), 30)), "control: surge pod unschedulable")
until(ns, lambda: False, 15)
check(old() and old()["spec"]["replicas"] == 1 and dep(ns)["status"].get("availableReplicas") == 1,
      "control: maxUnavailable 0 keeps the old pod (floor 1), as upstream")
sys.exit(failed)
PY
report
