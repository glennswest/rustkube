#!/usr/bin/env bash
#
# StatefulSet ordinals (#363, review finding c1b3be631b68), on a real
# apiserver and controller-manager on fastetcd. No kubelet: the rig marks
# each new Pod Running and Ready through /status.
#
# - replicas 3 come up in order: db-0, db-1, db-2
# - db-1 deleted: db-1 is made again (the lowest missing ordinal), never a
#   db-3, and db-2 is not re-created
# - db-2 Failed: deleted and made again
# - replicas 1: db-2 then db-1 go, db-0 stays
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
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
    try: return r.status, json.loads(raw)
    except ValueError: return r.status, raw
NS = "/api/v1/namespaces/default"
STS = "/apis/apps/v1/namespaces/default/statefulsets"
def pods():
    items = req("GET", NS + "/pods?labelSelector=app%3Ddb")[1]["items"]
    return {p["metadata"]["name"]: p for p in items if not p["metadata"].get("deletionTimestamp")}
def set_phase(p, phase, ready):
    p["status"] = {"phase": phase, "podIP": "10.98.0.1", "conditions": [{"type": "Ready", "status": "True" if ready else "False"}]}
    req("PUT", f"{NS}/pods/{p['metadata']['name']}/status", p)
def kubelet():
    # Every Pod not yet Running becomes Running and Ready.
    for p in pods().values():
        if p.get("status", {}).get("phase") not in ("Running", "Failed"):
            set_phase(p, "Running", True)
def until(f, secs=40):
    end = time.monotonic() + secs
    while time.monotonic() < end:
        kubelet()
        v = f()
        if v: return v
        time.sleep(0.5)
    return f()
created = []
code, out = req("POST", STS, {"apiVersion": "apps/v1", "kind": "StatefulSet", "metadata": {"name": "db"},
    "spec": {"replicas": 3, "serviceName": "db", "selector": {"matchLabels": {"app": "db"}},
             "template": {"metadata": {"labels": {"app": "db"}}, "spec": {"containers": [{"name": "c", "image": "pause"}]}}}})
if code != 201: print(f"setup: {code} {out}"); sys.exit(100)
def seen():
    for n in sorted(pods(), key=lambda n: pods()[n]["metadata"]["creationTimestamp"]):
        if n not in created: created.append(n)
    return sorted(pods()) == ["db-0", "db-1", "db-2"]
check(until(seen), f"replicas 3: db-0, db-1, db-2 ({sorted(pods())})")
check(created == ["db-0", "db-1", "db-2"], f"…made in order ({created})")

uid2 = pods()["db-2"]["metadata"]["uid"]
old1 = pods()["db-1"]["metadata"]["uid"]
req("DELETE", f"{NS}/pods/db-1", {"gracePeriodSeconds": 0})
check(until(lambda: "db-1" in pods() and pods()["db-1"]["metadata"]["uid"] != old1),
      f"db-1 deleted: db-1 is made again ({sorted(pods())})")
check(sorted(pods()) == ["db-0", "db-1", "db-2"] and pods()["db-2"]["metadata"]["uid"] == uid2,
      f"…no db-3, db-2 untouched ({sorted(pods())})")

set_phase(pods()["db-2"], "Failed", False)
check(until(lambda: "db-2" in pods() and pods()["db-2"]["metadata"]["uid"] != uid2
            and pods()["db-2"]["status"].get("phase") == "Running"),
      f"db-2 Failed: deleted and made again ({sorted(pods())})")

code, _ = req("PATCH", STS + "/db", {"spec": {"replicas": 1}}, "application/merge-patch+json")
check(code == 200 and until(lambda: sorted(pods()) == ["db-0"]), f"replicas 1: only db-0 is left ({sorted(pods())})")
sys.exit(failed)
PY
report
