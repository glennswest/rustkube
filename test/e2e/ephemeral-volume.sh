#!/usr/bin/env bash
#
# The ephemeral-volume controller (#94), against a real apiserver and
# controller-manager on fastetcd (no kubelet; the claim is the evidence).
#
# - a Pod with a generic ephemeral volume gets PVC `<pod>-<volume>` with the
#   template's labels, annotations and spec, owned by the Pod (controller)
# - a Pod whose `<pod>-<volume>` PVC already exists and is not its own:
#   that PVC is left as it is, and the Pod gets a Warning Event saying so
# - deleting the Pod: the garbage collector deletes its claim
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
NS = "/api/v1/namespaces/default"
tpl = {"metadata": {"labels": {"type": "scratch"}, "annotations": {"note": "eph"}},
       "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": "standard", "resources": {"requests": {"storage": "1Gi"}}}}
def pod(name):
    return req("POST", NS + "/pods", {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name},
        "spec": {"containers": [{"name": "c", "image": "pause", "volumeMounts": [{"name": "data", "mountPath": "/data"}]}],
                 "volumes": [{"name": "data", "ephemeral": {"volumeClaimTemplate": tpl}}]}})[1]
p = pod("web")
claim = until(lambda: (lambda c, o: o if c == 200 else None)(*req("GET", NS + "/persistentvolumeclaims/web-data")))
check(claim is not None, "claim web-data created for the Pod's ephemeral volume")
if claim:
    owner = claim["metadata"].get("ownerReferences", [{}])[0]
    check(owner.get("kind") == "Pod" and owner.get("uid") == p["metadata"]["uid"] and owner.get("controller") is True,
          f"owned by the Pod, as controller ({owner})")
    check(claim["metadata"].get("labels", {}).get("type") == "scratch" and claim["metadata"].get("annotations", {}).get("note") == "eph"
          and claim["spec"].get("storageClassName") == "standard" and claim["spec"]["resources"]["requests"]["storage"] == "1Gi",
          "labels, annotations and spec from the template")

req("POST", NS + "/persistentvolumeclaims", {"apiVersion": "v1", "kind": "PersistentVolumeClaim",
    "metadata": {"name": "other-data", "labels": {"mine": "yes"}},
    "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}}})
o = pod("other")
time.sleep(3)
code, foreign = req("GET", NS + "/persistentvolumeclaims/other-data")
check(code == 200 and not foreign["metadata"].get("ownerReferences") and foreign["metadata"]["labels"] == {"mine": "yes"},
      "an existing claim the Pod does not own is left alone, not adopted")
ev = until(lambda: [e for e in req("GET", NS + "/events")[1]["items"]
                    if e["involvedObject"].get("name") == "other" and e.get("type") == "Warning" and "pod is not owner" in e.get("message", "")])
check(bool(ev), f"…and the Pod gets a Warning Event ({ev[0]['message'] if ev else None})")

req("DELETE", NS + "/pods/web", {"gracePeriodSeconds": 0})
check(until(lambda: req("GET", NS + "/persistentvolumeclaims/web-data")[0] == 404, 60), "deleting the Pod: its claim is collected")
sys.exit(failed)
PY
report
