#!/usr/bin/env bash
#
# resource.k8s.io/v1 — Dynamic Resource Allocation's objects — served (#137),
# what the conformance suite's four `[DRA] CRUD Tests resource.k8s.io/v1`
# specs exercise. A real apiserver on fastetcd, no controllers.
#
# - /apis lists resource.k8s.io; /apis/resource.k8s.io/v1 lists
#   deviceclasses, resourceclaims (+ /status), resourceclaimtemplates,
#   resourceslices with their scope and verbs
# - each kind: create, get, list (and all namespaces), watch sees an ADDED,
#   merge patch, update, delete; deletecollection by label
# - ResourceClaim /status: an allocation written there, spec untouched
# - a protobuf GET (application/vnd.kubernetes.protobuf) answers protobuf
#
# Nothing allocates (no scheduler plugin, claim controller or kubelet plugin
# API): that is checked by nothing here. Exit status is the number of failed
# checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, threading, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"])
CTX = ssl._create_unverified_context()
G = "/apis/resource.k8s.io/v1"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(method, path, body=None, ctype="application/json", accept="application/json"):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype, "Accept": accept})
    r = conn.getresponse(); raw = r.read(); conn.close()
    if accept.startswith("application/json"):
        try: return r.status, json.loads(raw) if raw else None
        except ValueError: return r.status, raw
    return r.status, raw

_, apis = req("GET", "/apis")
check(any(g["name"] == "resource.k8s.io" for g in apis["groups"]), "/apis lists resource.k8s.io")
code, disc = req("GET", G)
res = {r["name"]: r for r in (disc or {}).get("resources", [])} if code == 200 else {}
want = {"deviceclasses": False, "resourceclaims": True, "resourceclaimtemplates": True, "resourceslices": False}
check(all(n in res and res[n]["namespaced"] == ns and "deletecollection" in res[n]["verbs"] for n, ns in want.items())
      and "resourceclaims/status" in res, f"{G} lists the four kinds and resourceclaims/status ({sorted(res)})")

req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "dra"}})
objs = {
    "deviceclasses": (G + "/deviceclasses", {"apiVersion": "resource.k8s.io/v1", "kind": "DeviceClass",
        "metadata": {"name": "gpu.example.com", "labels": {"t": "dra"}},
        "spec": {"selectors": [{"cel": {"expression": "device.driver == \"gpu.example.com\""}}]}}),
    "resourceslices": (G + "/resourceslices", {"apiVersion": "resource.k8s.io/v1", "kind": "ResourceSlice",
        "metadata": {"name": "node-a-gpu", "labels": {"t": "dra"}},
        "spec": {"driver": "gpu.example.com", "nodeName": "node-a",
                 "pool": {"name": "node-a", "generation": 1, "resourceSliceCount": 1},
                 "devices": [{"name": "gpu-0", "attributes": {"model": {"string": "x1"}}}]}}),
    "resourceclaims": (G + "/namespaces/dra/resourceclaims", {"apiVersion": "resource.k8s.io/v1", "kind": "ResourceClaim",
        "metadata": {"name": "c1", "labels": {"t": "dra"}},
        "spec": {"devices": {"requests": [{"name": "gpu", "exactly": {"deviceClassName": "gpu.example.com"}}]}}}),
    "resourceclaimtemplates": (G + "/namespaces/dra/resourceclaimtemplates", {"apiVersion": "resource.k8s.io/v1",
        "kind": "ResourceClaimTemplate", "metadata": {"name": "t1", "labels": {"t": "dra"}},
        "spec": {"spec": {"devices": {"requests": [{"name": "gpu", "exactly": {"deviceClassName": "gpu.example.com"}}]}}}}),
}

for kind, (coll, obj) in objs.items():
    seen = []
    def watch():
        conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=15)
        conn.request("GET", f"{coll}?watch=true&resourceVersion=0",
                     headers={"Authorization": "Bearer " + os.environ["ADMIN"]})
        r = conn.getresponse()
        try:
            for line in r:
                ev = json.loads(line)
                seen.append((ev["type"], ev["object"]["metadata"].get("name")))
        except Exception:
            pass
    t = threading.Thread(target=watch, daemon=True); t.start(); time.sleep(0.5)
    name = obj["metadata"]["name"]
    code, created = req("POST", coll, obj)
    check(code == 201 and created["kind"] == obj["kind"] and created["apiVersion"] == "resource.k8s.io/v1",
          f"{kind}: create ({code})")
    code, got = req("GET", f"{coll}/{name}")
    check(code == 200 and got["metadata"]["uid"] == created["metadata"]["uid"], f"{kind}: get")
    _, lst = req("GET", coll)
    check(lst.get("kind") == obj["kind"] + "List" and [i["metadata"]["name"] for i in lst["items"]] == [name],
          f"{kind}: list is a {lst.get('kind')}")
    _, all_ = req("GET", f"{G}/{kind}")
    check(any(i["metadata"]["name"] == name for i in all_.get("items", [])), f"{kind}: listed across namespaces")
    code, p = req("PATCH", f"{coll}/{name}", {"metadata": {"annotations": {"patched": "yes"}}}, "application/merge-patch+json")
    check(code == 200 and p["metadata"]["annotations"]["patched"] == "yes", f"{kind}: merge patch")
    p["metadata"]["labels"]["updated"] = "yes"
    code, u = req("PUT", f"{coll}/{name}", p)
    check(code == 200 and u["metadata"]["labels"].get("updated") == "yes", f"{kind}: update")
    time.sleep(1)
    check(("ADDED", name) in seen, f"{kind}: a watch sees the ADDED ({seen[:3]})")
    code, pb = req("GET", f"{coll}/{name}", accept="application/vnd.kubernetes.protobuf")
    check(code == 200 and isinstance(pb, bytes) and pb[:4] == b"k8s\x00", f"{kind}: protobuf GET ({code}, {pb[:4]!r})")

# ResourceClaim status: an allocation, as a scheduler would write it.
c1 = f"{G}/namespaces/dra/resourceclaims/c1"
_, claim = req("GET", c1)
claim["status"] = {"allocation": {"devices": {"results": [
    {"request": "gpu", "driver": "gpu.example.com", "pool": "node-a", "device": "gpu-0"}]}}}
claim["spec"]["devices"]["requests"][0]["name"] = "changed"
code, st = req("PUT", c1 + "/status", claim)
check(code == 200 and st["status"]["allocation"]["devices"]["results"][0]["device"] == "gpu-0"
      and st["spec"]["devices"]["requests"][0]["name"] == "gpu", f"resourceclaims/status: allocation written, spec kept ({code})")
code, g = req("GET", c1 + "/status")
check(code == 200 and g["status"].get("allocation"), "resourceclaims/status: get")

# Deletes.
for kind, (coll, obj) in objs.items():
    code, _ = req("DELETE", f"{coll}/{obj['metadata']['name']}")
    check(code in (200, 202) and req("GET", f"{coll}/{obj['metadata']['name']}")[0] == 404, f"{kind}: delete")
for i in range(3):
    req("POST", f"{G}/deviceclasses", {"apiVersion": "resource.k8s.io/v1", "kind": "DeviceClass",
        "metadata": {"name": f"dc{i}", "labels": {"bulk": "yes"}}, "spec": {}})
code, _ = req("DELETE", f"{G}/deviceclasses?labelSelector=bulk%3Dyes")
_, left = req("GET", f"{G}/deviceclasses?labelSelector=bulk%3Dyes")
check(code == 200 and left["items"] == [], f"deviceclasses: deletecollection by label ({len(left['items'])} left)")
sys.exit(failed)
PY
report
