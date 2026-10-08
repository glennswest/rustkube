#!/usr/bin/env bash
#
# The GC's orphan propagation under load (#159): the conformance spec "should
# orphan RS created by deployment when deleteOptions.PropagationPolicy is
# Orphan", run 20 times at once while other namespaces churn Deployments,
# against a real apiserver and controller-manager on fastetcd.
#
# Per run it keeps what the issue asks for: the ReplicaSet's uid and
# resourceVersion before and after, its owner references, and the time from
# the delete to the Deployment's disappearance. It separates the two ways the
# spec can fail:
# - unsafe deletion: any ReplicaSet of an orphaned Deployment deleted (its
#   uid gone, or a DELETED event for it on the watch) — must be 0;
# - latency: a Deployment not gone within the spec's 2 minutes — must be 0;
#   the slowest is reported.
# Then the orphaned ReplicaSets carry no reference to their old owner, and
# are still there 15 s later.
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
export API ADMIN
python3 - <<'PY' || FAIL=$?
import concurrent.futures, http.client, json, os, ssl, sys, threading, time, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
RUNS, failed = 20, 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=60)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": "application/json"})
    r = c.getresponse(); raw = r.read(); c.close()
    try: return r.status, json.loads(raw)
    except ValueError: return r.status, raw
def ns(n): req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": n}})
def deploy(n, name, label):
    return req("POST", f"/apis/apps/v1/namespaces/{n}/deployments", {"apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": name}, "spec": {"replicas": 0, "selector": {"matchLabels": {"app": label}},
        "template": {"metadata": {"labels": {"app": label}}, "spec": {"containers": [{"name": "c", "image": "pause"}]}}}})[1]
ns("orphan")
# Watch every ReplicaSet DELETED in the namespace for the whole test.
deleted, stop = [], threading.Event()
def watch():
    rv = req("GET", "/apis/apps/v1/namespaces/orphan/replicasets")[1]["metadata"]["resourceVersion"]
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=600)
    c.request("GET", f"/apis/apps/v1/namespaces/orphan/replicasets?watch=true&resourceVersion={rv}",
              headers={"Authorization": "Bearer " + os.environ["ADMIN"]})
    r = c.getresponse()
    while not stop.is_set():
        line = r.fp.readline()
        if not line: break
        ev = json.loads(line)
        if ev["type"] == "DELETED": deleted.append(ev["object"]["metadata"]["uid"])
threading.Thread(target=watch, daemon=True).start()
# Churn: other namespaces creating and deleting Deployments throughout.
def churn():
    i = 0
    while not stop.is_set():
        n = f"churn-{i % 4}"; ns(n)
        d = deploy(n, f"d{i}", f"c{i}")
        req("DELETE", f"/apis/apps/v1/namespaces/{n}/deployments/d{i}")
        i += 1
threading.Thread(target=churn, daemon=True).start()
time.sleep(1)

def run(i):
    name = f"orphan-{i}"
    d = deploy("orphan", name, name)
    t0, rs = time.monotonic(), None
    while time.monotonic() - t0 < 60:
        items = [x for x in req("GET", f"/apis/apps/v1/namespaces/orphan/replicasets?labelSelector=app%3D{name}")[1]["items"]]
        if items: rs = items[0]; break
        time.sleep(0.5)
    if rs is None: return {"run": i, "error": "no ReplicaSet within 60 s"}
    before = {"uid": rs["metadata"]["uid"], "rv": rs["metadata"]["resourceVersion"], "owners": rs["metadata"].get("ownerReferences")}
    req("DELETE", f"/apis/apps/v1/namespaces/orphan/deployments/{name}",
        {"kind": "DeleteOptions", "apiVersion": "v1", "propagationPolicy": "Orphan", "preconditions": {"uid": d["metadata"]["uid"]}})
    t1, gone = time.monotonic(), None
    while time.monotonic() - t1 < 120:
        if req("GET", f"/apis/apps/v1/namespaces/orphan/deployments/{name}")[0] == 404: gone = time.monotonic() - t1; break
        time.sleep(0.5)
    code, after = req("GET", f"/apis/apps/v1/namespaces/orphan/replicasets/{rs['metadata']['name']}")
    return {"run": i, "rs": rs["metadata"]["name"], "before": before, "gone_s": gone,
            "after": {"code": code, "uid": after.get("metadata", {}).get("uid") if code == 200 else None,
                      "rv": after.get("metadata", {}).get("resourceVersion") if code == 200 else None,
                      "owners": after.get("metadata", {}).get("ownerReferences") if code == 200 else None},
            "deployment_uid": d["metadata"]["uid"]}
with concurrent.futures.ThreadPoolExecutor(RUNS) as ex:
    results = list(ex.map(run, range(RUNS)))
time.sleep(15)
stop.set()
for r in results: print("RUN  " + json.dumps(r), flush=True)
errors = [r for r in results if "error" in r]
check(not errors, f"every Deployment made its ReplicaSet ({errors})")
ok = [r for r in results if "error" not in r]
lost = [r for r in ok if r["after"]["uid"] != r["before"]["uid"]]
watched = [r for r in ok if r["before"]["uid"] in deleted]
check(not lost and not watched, f"unsafe deletion: no orphaned ReplicaSet deleted ({len(lost)} gone, {len(watched)} DELETED on the watch)")
slow = [r for r in ok if r["gone_s"] is None]
times = sorted(r["gone_s"] for r in ok if r["gone_s"] is not None)
fmt = lambda t: f"{t:.1f} s" if t is not None else "n/a"
check(not slow, f"latency: every Deployment gone within 120 s ({len(slow)} not; slowest {fmt(times[-1] if times else None)}, median {fmt(times[len(times)//2] if times else None)})")
still_owned = [r for r in ok if any(o.get("uid") == r["deployment_uid"] for o in (r["after"]["owners"] or []))]
check(not still_owned, f"orphaned ReplicaSets carry no reference to the deleted Deployment ({len(still_owned)} still do)")
present = [r for r in ok if req("GET", f"/apis/apps/v1/namespaces/orphan/replicasets/{r['rs']}")[0] == 200]
check(len(present) == len(ok), f"…and are still there 15 s later ({len(present)}/{len(ok)})")
sys.exit(failed)
PY
grep -E "gc: |orphan" "$W/cm.log" | tail -40
report
