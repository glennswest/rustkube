#!/usr/bin/env bash
#
# ResourceQuota (#124): the controller computing status and the admission
# refusing and charging, against a real apiserver and controller-manager on
# fastetcd (no kubelet; Pods stay Pending, which counts).
#
# - status.hard mirrors spec.hard and status.used is computed promptly
# - a Pod within quota is admitted and charged at once (pods, requests.cpu)
# - one over requests.cpu: 403 "exceeded quota"; one without a cpu request:
#   403 "must specify requests.cpu"
# - services, secrets, configmaps and count/replicasets.apps are counted and
#   capped
# - deleting a Pod lowers status.used
# - a BestEffort-scoped quota counts BestEffort Pods only
# - six concurrent Pod creates against pods=3 admit exactly three
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
export API ADMIN
python3 - <<'PY' || FAIL=$?
import concurrent.futures, http.client, json, os, ssl, sys, time, urllib.parse
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
def ns(n):
    req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": n}})
    return f"/api/v1/namespaces/{n}"
def used(path, k):
    return req("GET", path)[1].get("status", {}).get("used", {}).get(k)
N = ns("quota-e2e")
Q = N + "/resourcequotas/q"
hard = {"pods": "2", "requests.cpu": "1", "services": "1", "secrets": "1", "count/replicasets.apps": "1"}
req("POST", N + "/resourcequotas", {"apiVersion": "v1", "kind": "ResourceQuota", "metadata": {"name": "q"}, "spec": {"hard": hard}})
st = until(lambda: (lambda s: s if s.get("hard") and s.get("used") else None)(req("GET", Q)[1].get("status", {})))
check(st is not None and st["hard"] == hard and st["used"].get("pods") == "0" and st["used"].get("services") == "0",
      f"status computed: hard mirrors spec, used 0 ({st})")
def pod(name, cpu="500m", path=N, **extra):
    res = {"requests": {"cpu": cpu}} if cpu else {}
    return req("POST", path + "/pods", {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name},
                                         "spec": dict({"containers": [{"name": "c", "image": "pause", "resources": res}]}, **extra)})
code, _ = pod("p1")
check(code == 201 and used(Q, "pods") == "1" and used(Q, "requests.cpu") == "500m", f"a Pod within quota: admitted and charged at once ({code}, {used(Q, 'requests.cpu')})")
code, out = pod("p2", "600m")
check(code == 403 and "exceeded quota: q, requested: requests.cpu=600m, used: requests.cpu=500m, limited: requests.cpu=1" in out.get("message", ""),
      f"over requests.cpu: 403 ({code} {out.get('message')})")
code, out = pod("p3", None)
check(code == 403 and "must specify requests.cpu" in out.get("message", ""), f"no cpu request: 403 ({code} {out.get('message')})")
for kind, path, body in [("services", "/services", {"apiVersion": "v1", "kind": "Service", "spec": {"ports": [{"port": 80}]}}),
                         ("secrets", "/secrets", {"apiVersion": "v1", "kind": "Secret", "data": {}})]:
    b1 = dict(body, metadata={"name": f"{kind}-1"}); b2 = dict(body, metadata={"name": f"{kind}-2"})
    c1, _ = req("POST", N + path, b1); c2, o2 = req("POST", N + path, b2)
    check(c1 == 201 and c2 == 403 and "exceeded quota" in o2.get("message", ""), f"{kind}: first admitted, second 403 ({c1}, {c2})")
rs = lambda n: {"apiVersion": "apps/v1", "kind": "ReplicaSet", "metadata": {"name": n}, "spec": {"replicas": 0,
     "selector": {"matchLabels": {"rs": n}}, "template": {"metadata": {"labels": {"rs": n}}, "spec": {"containers": [{"name": "c", "image": "pause"}]}}}}
c1, _ = req("POST", "/apis/apps/v1/namespaces/quota-e2e/replicasets", rs("r1"))
c2, _ = req("POST", "/apis/apps/v1/namespaces/quota-e2e/replicasets", rs("r2"))
check(c1 == 201 and c2 == 403 and until(lambda: used(Q, "count/replicasets.apps") == "1"), f"count/replicasets.apps: one admitted, the second 403 ({c1}, {c2})")
req("DELETE", N + "/pods/p1", {"gracePeriodSeconds": 0})
check(until(lambda: used(Q, "pods") == "0" and used(Q, "requests.cpu") == "0"), f"deleting the Pod lowers used ({used(Q, 'pods')}, {used(Q, 'requests.cpu')})")

B = ns("quota-be")
req("POST", B + "/resourcequotas", {"apiVersion": "v1", "kind": "ResourceQuota", "metadata": {"name": "be"},
                                    "spec": {"hard": {"pods": "1"}, "scopes": ["BestEffort"]}})
until(lambda: req("GET", B + "/resourcequotas/be")[1].get("status", {}).get("used"))
c1, _ = pod("burst", "100m", B)
c2, _ = pod("be-1", None, B)
c3, o3 = pod("be-2", None, B)
check(c1 == 201 and c2 == 201 and c3 == 403, f"BestEffort scope: burstable not counted, second BestEffort 403 ({c1}, {c2}, {c3})")

C = ns("quota-race")
req("POST", C + "/resourcequotas", {"apiVersion": "v1", "kind": "ResourceQuota", "metadata": {"name": "r"}, "spec": {"hard": {"pods": "3"}}})
until(lambda: req("GET", C + "/resourcequotas/r")[1].get("status", {}).get("used"))
with concurrent.futures.ThreadPoolExecutor(6) as ex:
    codes = list(ex.map(lambda i: pod(f"race-{i}", None, C)[0], range(6)))
check(codes.count(201) == 3 and codes.count(403) == 3, f"six concurrent creates against pods=3: exactly three admitted ({sorted(codes)})")
check(until(lambda: used(C + "/resourcequotas/r", "pods") == "3"), "…and used settles at 3")
sys.exit(failed)
PY
report
