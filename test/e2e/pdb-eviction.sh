#!/usr/bin/env bash
#
# Evictions against a PodDisruptionBudget (#360, review findings
# 6d98e18d1259 and b8d6824b0231), on a real apiserver and controller-manager
# on fastetcd. No kubelet: the Pods are made Running and Ready through
# /status.
#
# - four Ready Pods under minAvailable 2: the controller reports
#   disruptionsAllowed 2
# - four evictions released at the same instant: exactly two are 201 and two
#   429 (TooManyRequests, cause DisruptionBudget); two Pods are left
# - the controller's recount keeps the budget spent: currentHealthy 2,
#   disruptionsAllowed 0, and no disruptedPods record outlives its Pod
# - one more eviction is a 429
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, threading, time, urllib.parse
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
PDB = "/apis/policy/v1/namespaces/default/poddisruptionbudgets/web"
names = [f"web-{i}" for i in range(4)]
for n in names:
    code, p = req("POST", NS + "/pods", {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": n, "labels": {"app": "web"}},
                                        "spec": {"containers": [{"name": "c", "image": "pause"}]}})
    if code != 201: print(f"setup {n}: {code} {p}"); sys.exit(100)
    p["status"] = {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}
    code, out = req("PUT", f"{NS}/pods/{n}/status", p)
    if code != 200: print(f"setup {n} Ready: {code} {out}"); sys.exit(100)
code, out = req("POST", "/apis/policy/v1/namespaces/default/poddisruptionbudgets", {"apiVersion": "policy/v1",
    "kind": "PodDisruptionBudget", "metadata": {"name": "web"}, "spec": {"minAvailable": 2, "selector": {"matchLabels": {"app": "web"}}}})
if code != 201: print(f"setup pdb: {code} {out}"); sys.exit(100)
st = lambda: req("GET", PDB)[1].get("status", {})
check(until(lambda: st().get("disruptionsAllowed") == 2 and st().get("observedGeneration") is not None),
      f"four Ready Pods under minAvailable 2: disruptionsAllowed 2 ({st()})")

start = threading.Barrier(len(names)); codes = {}
def evict(n):
    start.wait()
    codes[n] = req("POST", f"{NS}/pods/{n}/eviction", {"apiVersion": "policy/v1", "kind": "Eviction", "metadata": {"name": n}})
threads = [threading.Thread(target=evict, args=(n,)) for n in names]
for t in threads: t.start()
for t in threads: t.join()
got = sorted(c for c, _ in codes.values())
check(got == [201, 201, 429, 429], f"four simultaneous evictions: two 201, two 429 ({got})")
refused = [o for c, o in codes.values() if c == 429]
check(all(isinstance(o, dict) and o.get("reason") == "TooManyRequests"
          and o.get("details", {}).get("causes", [{}])[0].get("reason") == "DisruptionBudget" for o in refused),
      f"the 429s are TooManyRequests with a DisruptionBudget cause ({refused[:1]})")
left = [n for n in names if req("GET", f"{NS}/pods/{n}")[0] == 200]
check(len(left) == 2, f"two Pods are left ({left})")
check(until(lambda: (lambda s: s.get("currentHealthy") == 2 and s.get("disruptionsAllowed") == 0
                     and not s.get("disruptedPods"))(st())),
      f"the recount keeps the budget spent, no stale record ({st()})")
code, _ = req("POST", f"{NS}/pods/{left[0]}/eviction", {"apiVersion": "policy/v1", "kind": "Eviction", "metadata": {"name": left[0]}}) if left else (0, None)
check(code == 429, f"one more eviction is a 429 ({code})")
sys.exit(failed)
PY
report
