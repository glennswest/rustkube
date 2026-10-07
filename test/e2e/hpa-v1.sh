#!/usr/bin/env bash
#
# autoscaling/v1 HorizontalPodAutoscaler (#123): a view of the stored v2
# object, converted both ways. A real apiserver on fastetcd, no controllers.
#
# - /apis lists autoscaling v2 (preferred) and v1; /apis/autoscaling/v1
#   lists horizontalpodautoscalers (+ /status) — the conformance Discovery spec
# - a v1 create with targetCPUUtilizationPercentage reads back from v2 as the
#   cpu Utilization metric
# - a v2 object with a memory metric and behavior reads as v1 with
#   targetCPUUtilizationPercentage and upstream's annotations
# - a v1 merge PATCH of targetCPUUtilizationPercentage changes v2's cpu
#   metric and keeps the memory metric; a v1 GET + PUT round trip leaves the
#   v2 spec as it was
# - list and watch through v1 carry autoscaling/v1 objects; delete through v1
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, threading, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
V1 = "/apis/autoscaling/v1/namespaces/default/horizontalpodautoscalers"
V2 = "/apis/autoscaling/v2/namespaces/default/horizontalpodautoscalers"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, ctype="application/json"):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype})
    r = c.getresponse(); raw = r.read(); c.close()
    return r.status, json.loads(raw) if raw else None

_, g = req("GET", "/apis")
auto = next(x for x in g["groups"] if x["name"] == "autoscaling")
check([v["version"] for v in auto["versions"]] == ["v2", "v1"] and auto["preferredVersion"]["version"] == "v2",
      f"/apis: autoscaling v2 (preferred) and v1 ({auto['versions']})")
_, rl = req("GET", "/apis/autoscaling/v1")
names = [r["name"] for r in rl["resources"]]
check("horizontalpodautoscalers" in names and "horizontalpodautoscalers/status" in names, f"/apis/autoscaling/v1 lists HPAs ({names})")

# Watch through v1 from now, in the background.
events = []
def watch():
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=20)
    c.request("GET", V1 + "?watch=1", headers={"Authorization": "Bearer " + os.environ["ADMIN"]})
    r = c.getresponse()
    for _ in range(2):
        line = r.fp.readline()
        if not line: break
        events.append(json.loads(line))
t = threading.Thread(target=watch, daemon=True); t.start()
import time; time.sleep(1)

ref = {"apiVersion": "apps/v1", "kind": "Deployment", "name": "web"}
code, out = req("POST", V1, {"apiVersion": "autoscaling/v1", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "one"},
    "spec": {"scaleTargetRef": ref, "minReplicas": 1, "maxReplicas": 4, "targetCPUUtilizationPercentage": 50}})
check(code == 201 and out["apiVersion"] == "autoscaling/v1" and out["spec"]["targetCPUUtilizationPercentage"] == 50, f"v1 create ({code})")
_, v2 = req("GET", f"{V2}/one")
m = v2["spec"].get("metrics", [{}])[0]
check(m.get("resource", {}).get("target", {}).get("averageUtilization") == 50 and m["resource"]["name"] == "cpu",
      f"stored as v2's cpu Utilization metric ({v2['spec'].get('metrics')})")

req("POST", V2, {"apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "two"},
    "spec": {"scaleTargetRef": ref, "maxReplicas": 6, "behavior": {"scaleDown": {"stabilizationWindowSeconds": 30}},
             "metrics": [{"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": 60}}},
                         {"type": "Resource", "resource": {"name": "memory", "target": {"type": "AverageValue", "averageValue": "100Mi"}}}]}})
code, v1 = req("GET", f"{V1}/two")
anns = v1["metadata"].get("annotations", {})
check(code == 200 and v1["spec"]["targetCPUUtilizationPercentage"] == 60 and "memory" in anns.get("autoscaling.alpha.kubernetes.io/metrics", "")
      and "stabilizationWindowSeconds" in anns.get("autoscaling.alpha.kubernetes.io/behavior", ""),
      f"v2 reads as v1: target 60, memory metric and behavior in annotations ({v1['spec']}, {list(anns)})")
before = req("GET", f"{V2}/two")[1]["spec"]
code, p = req("PATCH", f"{V1}/two", {"spec": {"targetCPUUtilizationPercentage": 70}}, "application/merge-patch+json")
after = req("GET", f"{V2}/two")[1]["spec"]
cpu = [x for x in after["metrics"] if x["resource"]["name"] == "cpu"]
check(code == 200 and cpu and cpu[0]["resource"]["target"]["averageUtilization"] == 70
      and any(x["resource"]["name"] == "memory" for x in after["metrics"]) and after.get("behavior") == before["behavior"],
      f"v1 merge PATCH: cpu 70, memory metric and behavior kept ({code} {after})")
_, cur = req("GET", f"{V1}/two")
code, _ = req("PUT", f"{V1}/two", cur)
check(code == 200 and req("GET", f"{V2}/two")[1]["spec"] == after, f"v1 GET + PUT leaves the v2 spec as it was ({code})")

code, l = req("GET", V1)
check(code == 200 and l["apiVersion"] == "autoscaling/v1" and all(i["apiVersion"] == "autoscaling/v1" for i in l["items"]) and len(l["items"]) == 2,
      f"v1 list ({code})")
t.join(15)
check(len(events) >= 1 and events[0]["object"]["apiVersion"] == "autoscaling/v1" and "targetCPUUtilizationPercentage" in events[0]["object"]["spec"],
      f"v1 watch carries v1 objects ({[e.get('type') for e in events]})")
code, _ = req("DELETE", f"{V1}/one")
check(code == 200 and req("GET", f"{V2}/one")[0] == 404, f"v1 delete ({code})")
sys.exit(failed)
PY
report
