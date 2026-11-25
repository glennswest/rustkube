#!/usr/bin/env bash
#
# minReadySeconds (#379) against a real apiserver, controller-manager and
# scheduler. Two stand-in nodes; the script plays the kubelet: a bound pod
# becomes Running and Ready, its Ready condition's lastTransitionTime the
# moment it did. Every workload here has minReadySeconds $MIN, so a pod is
# available only $MIN s after it is Ready, and the controllers must come back
# by themselves then: nothing else changes to wake them.
#
#   1. ReplicaSet: availableReplicas stays 0 while readyReplicas is 2, then
#      reaches 2 about $MIN s later
#   2. Deployment (maxSurge 1, maxUnavailable 0): its new ReplicaSet carries
#      minReadySeconds; on a template change the old ReplicaSet is not scaled
#      down until a new pod has been Ready $MIN s; the rollout completes
#   3. DaemonSet: numberAvailable lags numberReady by $MIN s; a template
#      change rolls the second node only once the first node's new pod is
#      available
#   4. StatefulSet (OrderedReady): web-1 is created only once web-0 is
#      available; availableReplicas reaches 2
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
start_scheduler
export API ADMIN W
MIN=${RK_MIN_READY:-6} python3 - <<'PY' || FAIL=$?
import datetime, http.client, json, os, ssl, sys, threading, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"]); TOKEN = os.environ["ADMIN"]
MIN = int(os.environ["MIN"])
# The Ready time is kept to the second, so allow one second short.
SLACK = 1.5
CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(method, path, body=None, ctype="application/json", ok=(200, 201, 404, 409, 422)):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + TOKEN, "Content-Type": ctype})
    r = conn.getresponse(); data = r.read()
    if r.status not in ok:
        print(f"setup {method} {path}: {r.status} {data[:300]!r}"); sys.exit(100)
    try:
        return r.status, json.loads(data)
    except ValueError:
        return r.status, data.decode(errors="replace")

def get(path):
    st, v = req("GET", path)
    return v if st == 200 else None

def now(frac=True):
    t = datetime.datetime.now(datetime.timezone.utc)
    return t.strftime("%Y-%m-%dT%H:%M:%S.%fZ" if frac else "%Y-%m-%dT%H:%M:%SZ")

NS, NODES = "minready", ["n1", "n2"]
req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": NS}})
res = {"cpu": "64", "memory": "256Gi", "pods": "110"}
for n in NODES:
    req("POST", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases", {"apiVersion": "coordination.k8s.io/v1",
        "kind": "Lease", "metadata": {"name": n}, "spec": {"holderIdentity": n, "leaseDurationSeconds": 40, "renewTime": now()}})
    _, node = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node",
        "metadata": {"name": n, "labels": {"kubernetes.io/hostname": n}}})
    node["status"] = {"capacity": res, "allocatable": res, "conditions": [{"type": "Ready", "status": "True"}]}
    req("PUT", f"/api/v1/nodes/{n}/status", node)

ready_at = {}  # pod name -> monotonic time it was made Ready
stop = threading.Event()
def kubelet():
    """Leases renewed; a bound pod Running and Ready from now (its Ready
    lastTransitionTime to the second, as a kubelet writes it); a
    terminating pod gone."""
    last = 0
    while not stop.is_set():
        try:
            if time.monotonic() - last > 5:
                for n in NODES:
                    l = get(f"/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/{n}")
                    l["spec"]["renewTime"] = now()
                    req("PUT", f"/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/{n}", l)
                last = time.monotonic()
            for p in get(f"/api/v1/namespaces/{NS}/pods")["items"]:
                name = p["metadata"]["name"]
                if p["metadata"].get("deletionTimestamp"):
                    req("DELETE", f"/api/v1/namespaces/{NS}/pods/{name}?gracePeriodSeconds=0")
                    continue
                if not p["spec"].get("nodeName") or p.get("status", {}).get("phase") == "Running":
                    continue
                t = now(frac=False)
                p["status"] = {"phase": "Running", "startTime": t, "conditions": [
                    {"type": "PodScheduled", "status": "True", "lastTransitionTime": t},
                    {"type": "Ready", "status": "True", "lastTransitionTime": t}]}
                st, _ = req("PUT", f"/api/v1/namespaces/{NS}/pods/{name}/status", p)
                if st == 200:
                    ready_at[name] = time.monotonic()
        except Exception as e:  # keep playing the kubelet
            print("kubelet:", e, flush=True)
        time.sleep(0.2)
threading.Thread(target=kubelet, daemon=True).start()

def until(f, limit=60, step=0.2):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        v = f()
        if v: return v
        time.sleep(step)
    return f()

def status(kind, name):
    group = "/api/v1" if kind == "replicationcontrollers" else "/apis/apps/v1"
    o = get(f"{group}/namespaces/{NS}/{kind}/{name}")
    return (o or {}).get("status", {})

def template(app, image="busybox:1"):
    return {"metadata": {"labels": {"app": app}}, "spec": {"containers": [{"name": "c", "image": image}]}}

def owned_by(kind, name):
    return [p for p in get(f"/api/v1/namespaces/{NS}/pods")["items"]
            if not p["metadata"].get("deletionTimestamp")
            and any(r.get("kind") == kind and r.get("name") == name for r in p["metadata"].get("ownerReferences", []))]

# 1. ReplicaSet
req("POST", f"/apis/apps/v1/namespaces/{NS}/replicasets", {"apiVersion": "apps/v1", "kind": "ReplicaSet",
    "metadata": {"name": "rs"}, "spec": {"replicas": 2, "minReadySeconds": MIN, "selector": {"matchLabels": {"app": "rs"}},
                                         "template": template("rs")}})
st = until(lambda: status("replicasets", "rs").get("readyReplicas") == 2 and status("replicasets", "rs"))
t_ready = max([ready_at.get(p["metadata"]["name"], 0) for p in owned_by("ReplicaSet", "rs")] or [0])
check(bool(st) and st.get("availableReplicas", 0) == 0,
      f"rs: readyReplicas 2, availableReplicas {st and st.get('availableReplicas')} before {MIN}s")
ok = until(lambda: status("replicasets", "rs").get("availableReplicas") == 2, MIN + 20)
waited = time.monotonic() - t_ready
check(bool(ok) and waited >= MIN - SLACK,
      f"rs: availableReplicas 2 after {waited:.1f}s of Ready (minReadySeconds {MIN}), with nothing else to wake it")

# 2. Deployment
req("POST", f"/apis/apps/v1/namespaces/{NS}/deployments", {"apiVersion": "apps/v1", "kind": "Deployment",
    "metadata": {"name": "d"}, "spec": {"replicas": 2, "minReadySeconds": MIN, "selector": {"matchLabels": {"app": "d"}},
        "strategy": {"type": "RollingUpdate", "rollingUpdate": {"maxSurge": 1, "maxUnavailable": 0}},
        "template": template("d")}})
def rss():
    return [r for r in get(f"/apis/apps/v1/namespaces/{NS}/replicasets")["items"]
            if any(o.get("name") == "d" for o in r["metadata"].get("ownerReferences", []))]
check(until(lambda: status("deployments", "d").get("availableReplicas") == 2, MIN + 40),
      f"d: the first rollout completes: {status('deployments', 'd')}")
first = rss()
check(len(first) == 1 and first[0]["spec"].get("minReadySeconds") == MIN,
      f"d: its ReplicaSet carries minReadySeconds {MIN}: {[r['spec'].get('minReadySeconds') for r in first]}")
old = first[0]["metadata"]["name"] if first else ""
req("PATCH", f"/apis/apps/v1/namespaces/{NS}/deployments/d",
    {"spec": {"template": {"spec": {"containers": [{"name": "c", "image": "busybox:2"}]}}}},
    ctype="application/strategic-merge-patch+json")
new_ready = old_down = None
end = time.monotonic() + MIN * 3 + 40
while time.monotonic() < end and old_down is None:
    for r in rss():
        n = r["metadata"]["name"]
        if n != old and new_ready is None and r.get("status", {}).get("readyReplicas", 0) >= 1:
            new_ready = min(ready_at.get(p["metadata"]["name"], time.monotonic()) for p in owned_by("ReplicaSet", n)) \
                if owned_by("ReplicaSet", n) else time.monotonic()
        if n == old and r["spec"].get("replicas", 2) < 2:
            old_down = time.monotonic()
    time.sleep(0.2)
gap = (old_down - new_ready) if (old_down and new_ready) else None
check(gap is not None and gap >= MIN - SLACK,
      f"d: the old ReplicaSet is scaled down {gap and round(gap, 1)}s after a new pod is Ready (minReadySeconds {MIN})")
check(until(lambda: (lambda s: s.get("updatedReplicas") == 2 and s.get("availableReplicas") == 2
                     and s.get("replicas") == 2)(status("deployments", "d")), MIN * 4 + 40),
      f"d: the rollout completes: {status('deployments', 'd')}")

# 3. DaemonSet
req("POST", f"/apis/apps/v1/namespaces/{NS}/daemonsets", {"apiVersion": "apps/v1", "kind": "DaemonSet",
    "metadata": {"name": "ds"}, "spec": {"minReadySeconds": MIN, "selector": {"matchLabels": {"app": "ds"}},
        "updateStrategy": {"type": "RollingUpdate", "rollingUpdate": {"maxUnavailable": 1}}, "template": template("ds")}})
st = until(lambda: status("daemonsets", "ds").get("numberReady") == 2 and status("daemonsets", "ds"))
t_ready = max([ready_at.get(p["metadata"]["name"], 0) for p in owned_by("DaemonSet", "ds")] or [0])
check(bool(st) and st.get("numberAvailable", 0) == 0 and st.get("numberUnavailable") == 2,
      f"ds: numberReady 2, numberAvailable {st and st.get('numberAvailable')} before {MIN}s")
ok = until(lambda: status("daemonsets", "ds").get("numberAvailable") == 2, MIN + 20)
waited = time.monotonic() - t_ready
check(bool(ok) and waited >= MIN - SLACK, f"ds: numberAvailable 2 after {waited:.1f}s of Ready")
before = {p["metadata"]["name"] for p in owned_by("DaemonSet", "ds")}
req("PATCH", f"/apis/apps/v1/namespaces/{NS}/daemonsets/ds",
    {"spec": {"template": {"spec": {"containers": [{"name": "c", "image": "busybox:2"}]}}}},
    ctype="application/strategic-merge-patch+json")
first_new_ready = second_old_gone = None
end = time.monotonic() + MIN * 3 + 40
while time.monotonic() < end and second_old_gone is None:
    pods = owned_by("DaemonSet", "ds")
    new = [p for p in pods if p["metadata"]["name"] not in before]
    if first_new_ready is None and new and any(p["metadata"]["name"] in ready_at for p in new):
        first_new_ready = min(ready_at[p["metadata"]["name"]] for p in new if p["metadata"]["name"] in ready_at)
    if len(before - {p["metadata"]["name"] for p in pods}) >= 2:
        second_old_gone = time.monotonic()
    time.sleep(0.2)
gap = (second_old_gone - first_new_ready) if (second_old_gone and first_new_ready) else None
check(gap is not None and gap >= MIN - SLACK,
      f"ds: the second node's old pod goes {gap and round(gap, 1)}s after the first new pod is Ready")
check(until(lambda: (lambda s: s.get("updatedNumberScheduled") == 2 and s.get("numberAvailable") == 2)(
          status("daemonsets", "ds")), MIN * 3 + 40), f"ds: the rollout completes: {status('daemonsets', 'ds')}")

# 4. StatefulSet
req("POST", f"/apis/apps/v1/namespaces/{NS}/statefulsets", {"apiVersion": "apps/v1", "kind": "StatefulSet",
    "metadata": {"name": "web"}, "spec": {"replicas": 2, "minReadySeconds": MIN, "serviceName": "web",
        "selector": {"matchLabels": {"app": "web"}}, "template": template("web")}})
until(lambda: "web-0" in ready_at)
created = until(lambda: get(f"/api/v1/namespaces/{NS}/pods/web-1") and time.monotonic(), MIN + 30)
gap = (created - ready_at["web-0"]) if (created and "web-0" in ready_at) else None
check(gap is not None and gap >= MIN - SLACK, f"web: web-1 is created {gap and round(gap, 1)}s after web-0 is Ready")
check(until(lambda: status("statefulsets", "web").get("availableReplicas") == 2, MIN * 2 + 30),
      f"web: availableReplicas 2: {status('statefulsets', 'web')}")

stop.set()
sys.exit(failed)
PY
report
