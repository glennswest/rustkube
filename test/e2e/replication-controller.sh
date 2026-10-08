#!/usr/bin/env bash
#
# The ReplicationController controller (#125), against a real apiserver and
# controller-manager on fastetcd, no kubelet (Pods stay Pending).
#
# - an RC without a selector is defaulted from its template (selector,
#   labels, replicas 1)
# - an RC of 2 makes 2 Pods owned by kind ReplicationController (controller),
#   with the template's labels; status replicas / fullyLabeledReplicas 2,
#   observedGeneration set
# - discovery lists replicationcontrollers/scale; GET /scale reads 2 with the
#   selector string; PUT /scale to 3 → 3 Pods; PATCH /scale to 1 → 1 Pod
# - a deleted Pod is replaced
# - delete with orphan propagation leaves the Pods without the owner; a
#   background delete (the GC conformance spec) removes them
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
NS = "/api/v1/namespaces/rc-e2e"
req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "rc-e2e"}})
def pods(app, owned_by=None):
    items = req("GET", f"{NS}/pods?labelSelector=app%3D{app}")[1]["items"]
    items = [p for p in items if not p["metadata"].get("deletionTimestamp")]
    if owned_by is not None:
        items = [p for p in items if any(r["uid"] == owned_by for r in p["metadata"].get("ownerReferences", []))]
    return items
def eventually(label, test, secs=30):
    until = time.monotonic() + secs
    while time.monotonic() < until:
        if test(): return check(True, label)
        time.sleep(0.3)
    check(False, label)
def rc(name, replicas=None, selector=None):
    spec = {"template": {"metadata": {"labels": {"app": name}},
                         "spec": {"containers": [{"name": "c", "image": "pause"}]}}}
    if replicas is not None: spec["replicas"] = replicas
    if selector is not None: spec["selector"] = selector
    return req("POST", NS + "/replicationcontrollers",
               {"apiVersion": "v1", "kind": "ReplicationController", "metadata": {"name": name}, "spec": spec})

code, d = rc("dflt")
check(code == 201 and d["spec"]["selector"] == {"app": "dflt"} and d["metadata"]["labels"] == {"app": "dflt"}
      and d["spec"]["replicas"] == 1, f"defaults from the template ({code})")
code, web = rc("web", 2, {"app": "web"})
uid = web["metadata"]["uid"]
eventually("RC of 2 makes 2 Pods it owns", lambda: len(pods("web", uid)) == 2)
p = pods("web", uid)
o = p[0]["metadata"]["ownerReferences"][0] if p else {}
check(o.get("kind") == "ReplicationController" and o.get("apiVersion") == "v1" and o.get("controller") is True,
      f"owner reference: kind ReplicationController, controller ({o})")
eventually("status replicas 2, fullyLabeledReplicas 2",
           lambda: (lambda s: s.get("replicas") == 2 and s.get("fullyLabeledReplicas") == 2 and "observedGeneration" in s)
                   (req("GET", NS + "/replicationcontrollers/web")[1].get("status", {})))

_, res = req("GET", "/api/v1")
check(any(r["name"] == "replicationcontrollers/scale" for r in res["resources"]), "discovery lists replicationcontrollers/scale")
code, sc = req("GET", NS + "/replicationcontrollers/web/scale")
check(code == 200 and sc["kind"] == "Scale" and sc["spec"]["replicas"] == 2 and sc["status"]["selector"] == "app=web",
      f"GET /scale ({code} {sc})")
sc["spec"]["replicas"] = 3
code, _ = req("PUT", NS + "/replicationcontrollers/web/scale", sc)
check(code == 200, f"PUT /scale to 3 ({code})")
eventually("…3 Pods", lambda: len(pods("web", uid)) == 3)
code, _ = req("PATCH", NS + "/replicationcontrollers/web/scale", {"spec": {"replicas": 1}}, "application/merge-patch+json")
check(code == 200, f"PATCH /scale to 1 ({code})")
eventually("…1 Pod", lambda: len(pods("web", uid)) == 1)
victim = pods("web", uid)[0]["metadata"]["name"]
req("DELETE", f"{NS}/pods/{victim}", {"gracePeriodSeconds": 0})
eventually("a deleted Pod is replaced", lambda: [x["metadata"]["name"] for x in pods("web", uid)] not in ([], [victim]))

code, orph = rc("orph", 2)
ouid = orph["metadata"]["uid"]
eventually("second RC makes its 2 Pods", lambda: len(pods("orph", ouid)) == 2)
req("DELETE", NS + "/replicationcontrollers/orph", {"propagationPolicy": "Orphan"})
eventually("orphan delete: the Pods stay, without the owner",
           lambda: req("GET", NS + "/replicationcontrollers/orph")[0] == 404 and len(pods("orph")) == 2 and len(pods("orph", ouid)) == 0)
req("DELETE", NS + "/replicationcontrollers/web", {"propagationPolicy": "Background"})
eventually("background delete: the GC removes its Pods", lambda: len(pods("web")) == 0, 60)
sys.exit(failed)
PY
report
