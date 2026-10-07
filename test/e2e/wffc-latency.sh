#!/usr/bin/env bash
#
# WaitForFirstConsumer claims to bound Pods, control-plane only (#147's PVC
# row; #142 handed the measurement there). A real apiserver,
# controller-manager and scheduler on fastetcd; a stand-in Node kept alive by
# its Lease; no kubelet, so node time is out of the number.
#
# 25 Pods, each with a fresh claim of the in-kubelet `stormblock` class
# (WaitForFirstConsumer), created at once — turbomode's SQLite profile on
# pvetest1 measured request→scheduled p50 1.42 / p95 2.54 / max 2.58 s for this.
# The chain: the scheduler picks the node and writes selected-node on the
# claim → the stormblock provisioner writes a PV pre-bound to it → the binder
# binds them → the scheduler binds the Pod. Time per Pod: from the create's
# acknowledgement to the scheduler's own `storm.io/scheduled-at` (µs, same
# clock). Checks: all 25 bound, claims Bound to `pvc-<ns>-<claim>`, and
# #147's control-plane target, p99 < 1 s.
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
start_scheduler
export API ADMIN
python3 - <<'PY' || FAIL=$?
import datetime, http.client, json, os, ssl, sys, threading, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"])
CTX = ssl._create_unverified_context()
N = 25
NS = "wffc"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(method, path, body=None, ctype="application/json"):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype})
    r = conn.getresponse(); raw = r.read(); conn.close()
    return r.status, json.loads(raw) if raw else None

def setup(r, what):
    if r[0] >= 300 and r[0] != 409:
        print(f"setup {what}: {r}"); sys.exit(100)
    return r[1]

def micro():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")

# A Node that stays Ready: its Lease first, renewed (the node controller marks
# a node with a stale or missing renewTime NotReady).
setup(req("POST", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases",
          {"apiVersion": "coordination.k8s.io/v1", "kind": "Lease", "metadata": {"name": "n1"},
           "spec": {"holderIdentity": "n1", "leaseDurationSeconds": 40, "renewTime": micro()}}), "lease")
def renew():
    while True:
        time.sleep(5)
        req("PATCH", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/n1",
            {"spec": {"renewTime": micro()}}, "application/merge-patch+json")
threading.Thread(target=renew, daemon=True).start()
node = setup(req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node",
             "metadata": {"name": "n1", "labels": {"kubernetes.io/hostname": "n1"}}}), "node")
res = {"cpu": "64", "memory": "256Gi", "pods": "250"}
node = setup(req("GET", "/api/v1/nodes/n1"), "node get")
node["status"] = {"capacity": res, "allocatable": res, "conditions": [{"type": "Ready", "status": "True"}]}
setup(req("PUT", "/api/v1/nodes/n1/status", node), "node status")
setup(req("POST", "/apis/storage.k8s.io/v1/storageclasses", {
    "apiVersion": "storage.k8s.io/v1", "kind": "StorageClass", "metadata": {"name": "stormblock"},
    "provisioner": "stormblock.storm.io/in-kubelet", "volumeBindingMode": "WaitForFirstConsumer"}), "class")
setup(req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": NS}}), "ns")
time.sleep(3)  # the controllers' informers see the class and the node

# Claims first, then the Pods at once, each Pod's create time noted.
for i in range(N):
    setup(req("POST", f"/api/v1/namespaces/{NS}/persistentvolumeclaims", {
        "apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": f"data-{i}"},
        "spec": {"storageClassName": "stormblock", "accessModes": ["ReadWriteOnce"],
                 "resources": {"requests": {"storage": "1Gi"}}}}), f"claim {i}")
time.sleep(2)  # protection finalizers, Pending: not part of the Pod's time
created = {}
def make(i):
    t0 = time.time()
    code, _ = req("POST", f"/api/v1/namespaces/{NS}/pods", {
        "apiVersion": "v1", "kind": "Pod", "metadata": {"name": f"p{i}"},
        "spec": {"tolerations": [{"operator": "Exists"}],
                 "containers": [{"name": "c", "image": "unused"}],
                 "volumes": [{"name": "d", "persistentVolumeClaim": {"claimName": f"data-{i}"}}]}})
    if code < 300:
        created[f"p{i}"] = time.time()
threads = [threading.Thread(target=make, args=(i,)) for i in range(N)]
for t in threads: t.start()
for t in threads: t.join()
check(len(created) == N, f"{N} Pods created ({len(created)})")

bound = {}
deadline = time.time() + 60
while time.time() < deadline and len(bound) < N:
    _, lst = req("GET", f"/api/v1/namespaces/{NS}/pods")
    for p in lst["items"]:
        n = p["metadata"]["name"]
        at = p["metadata"].get("annotations", {}).get("storm.io/scheduled-at")
        if p["spec"].get("nodeName") and at and n not in bound:
            bound[n] = datetime.datetime.fromisoformat(at.replace("Z", "+00:00")).timestamp()
    time.sleep(0.2)
check(len(bound) == N, f"all {N} Pods bound ({len(bound)})")
_, claims = req("GET", f"/api/v1/namespaces/{NS}/persistentvolumeclaims")
ok = [c for c in claims["items"] if c.get("status", {}).get("phase") == "Bound"
      and c["spec"].get("volumeName") == f"pvc-{NS}-{c['metadata']['name']}"]
check(len(ok) == N, f"claims Bound to their pvc-<ns>-<claim> volume ({len(ok)}/{N})")

lat = sorted(bound[n] - created[n] for n in bound if n in created)
if lat:
    pct = lambda q: lat[min(len(lat) - 1, int(round(q * (len(lat) - 1))))]
    print(f"create→bound s: p50 {pct(.5):.3f} p95 {pct(.95):.3f} p99 {pct(.99):.3f} max {lat[-1]:.3f} (n={len(lat)})", flush=True)
    check(pct(.99) < 1.0, f"control-plane p99 < 1 s (#147): {pct(.99):.3f} s")
sys.exit(failed)
PY
report
