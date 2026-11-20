#!/usr/bin/env bash
#
# A Service's Endpoints and EndpointSlices from its Pods (#362, review
# findings 7a974852b70e, 7d20cd929fba, 7ada61477e9f), on a real apiserver and
# controller-manager on fastetcd. No kubelet: Pods get their IP and Ready
# condition through /status.
#
# Service `web`: port 80, targetPort "http". Pod a names http 8080 and is
# not Ready; b names it 9090 and is Ready; c has no port named http.
# - Endpoints: a subset for 8080 with a under notReadyAddresses, one for 9090
#   with b under addresses; c in neither (no fallback to port 80)
# - two EndpointSlices, one per port: a `ready: false`, b `ready: true`
# - a turns Ready: it moves to addresses and its slice says ready
# - b stops matching: the 9090 subset and its slice go
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
def until(f, secs=30):
    end = time.monotonic() + secs
    while time.monotonic() < end:
        v = f()
        if v: return v
        time.sleep(0.3)
    return f()
NS = "/api/v1/namespaces/default"
code, out = req("POST", NS + "/services", {"apiVersion": "v1", "kind": "Service", "metadata": {"name": "web"},
    "spec": {"selector": {"app": "ep"}, "ports": [{"name": "web", "port": 80, "targetPort": "http"}]}})
if code != 201: print(f"setup service: {code} {out}"); sys.exit(100)
def pod(name, ip, port, ready):
    ports = [{"name": "http", "containerPort": port}] if port else []
    code, p = req("POST", NS + "/pods", {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "labels": {"app": "ep"}},
        "spec": {"containers": [{"name": "c", "image": "pause", "ports": ports}]}})
    if code != 201: print(f"setup {name}: {code} {p}"); sys.exit(100)
    set_status(name, ip, ready)
def set_status(name, ip, ready):
    p = req("GET", f"{NS}/pods/{name}")[1]
    p["status"] = {"phase": "Running", "podIP": ip, "podIPs": [{"ip": ip}],
                   "conditions": [{"type": "Ready", "status": "True" if ready else "False"}]}
    code, out = req("PUT", f"{NS}/pods/{name}/status", p)
    if code != 200: print(f"setup {name} status: {code} {out}"); sys.exit(100)
pod("a", "10.99.1.1", 8080, False)
pod("b", "10.99.1.2", 9090, True)
pod("c", "10.99.1.3", None, True)

def subsets():
    s = req("GET", NS + "/endpoints/web")[1].get("subsets", []) or []
    return {ss["ports"][0]["port"]: ([a["ip"] for a in ss.get("addresses", [])], [a["ip"] for a in ss.get("notReadyAddresses", [])])
            for ss in s}
def slices():
    items = req("GET", "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices?labelSelector=kubernetes.io%2Fservice-name%3Dweb")[1]["items"]
    return {(s.get("ports") or [{}])[0].get("port"): [(e["addresses"][0], e["conditions"].get("ready")) for e in s.get("endpoints") or []]
            for s in items}
want = {8080: ([], ["10.99.1.1"]), 9090: (["10.99.1.2"], [])}
check(until(lambda: subsets() == want), f"Endpoints: a subset per resolved port, a not ready, c nowhere ({subsets()})")
check(until(lambda: slices() == {8080: [("10.99.1.1", False)], 9090: [("10.99.1.2", True)]}),
      f"an EndpointSlice per port with the Pods' readiness ({slices()})")
set_status("a", "10.99.1.1", True)
check(until(lambda: subsets().get(8080) == (["10.99.1.1"], [])), f"a turns Ready: addresses ({subsets()})")
check(until(lambda: slices().get(8080) == [("10.99.1.1", True)]), f"…and its slice says ready ({slices()})")
code, _ = req("PATCH", NS + "/pods/b", {"metadata": {"labels": {"app": "other"}}}, "application/merge-patch+json")
check(until(lambda: list(subsets()) == [8080] and list(slices()) == [8080]),
      f"b stops matching: its subset and its slice go ({subsets()}, {slices()})")
sys.exit(failed)
PY
report
