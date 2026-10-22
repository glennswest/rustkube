#!/usr/bin/env bash
#
# A resource quantity sent as a JSON number is stored and returned as
# upstream's string (#292): a real apiserver on fastetcd. cilium's
# `install-cni-binaries` carries `limits.cpu: 1` (an unquoted YAML number);
# rustkube stored the number and k8s-openapi clients (stormupdate) refused the
# whole PodList. Create (JSON and YAML bodies), merge PATCH and PUT with
# numbers; GET and LIST return canonical strings (`1`, `500m`, `1k`), non-
# quantity numbers stay numbers, and LimitRange/ResourceQuota quantities are
# strings too. Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, ctype="application/json"):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    data = body if isinstance(body, str) or body is None else json.dumps(body)
    conn.request(method, path, body=data,
                 headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype})
    r = conn.getresponse(); raw = r.read(); conn.close()
    return r.status, json.loads(raw) if raw else None
def numbers_at_quantities(obj, path=()):
    """Every JSON number under a resources/limits/requests/hard/... map."""
    found = []
    if isinstance(obj, dict):
        for k, v in obj.items():
            if isinstance(v, (int, float)) and not isinstance(v, bool) and path and path[-1] in (
                    "limits", "requests", "hard", "used", "min", "max", "default", "defaultRequest", "overhead"):
                found.append("/".join(path + (k,)))
            else:
                found += numbers_at_quantities(v, path + (k,))
    elif isinstance(obj, list):
        for v in obj:
            found += numbers_at_quantities(v, path)
    return found

P = "/api/v1/namespaces/default/pods"
code, out = req("POST", P, {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "q1"},
    "spec": {"initContainers": [{"name": "install-cni-binaries", "image": "busybox",
        "resources": {"limits": {"cpu": 1, "memory": "1Gi"}, "requests": {"cpu": "100m", "memory": "10Mi"}}}],
     "containers": [{"name": "c", "image": "busybox", "ports": [{"containerPort": 8080}],
        "resources": {"requests": {"cpu": 0.5, "memory": 1000}}}],
     "terminationGracePeriodSeconds": 7}})
check(code == 201, f"create with numeric quantities accepted ({code})")
init = out["spec"]["initContainers"][0]["resources"] if code == 201 else {}
check(init.get("limits") == {"cpu": "1", "memory": "1Gi"}, f"create answers limits.cpu \"1\" ({init.get('limits')})")
_, got = req("GET", f"{P}/q1")
c = got["spec"]["containers"][0]
check(got["spec"]["initContainers"][0]["resources"]["limits"]["cpu"] == "1", "GET: init limits.cpu \"1\"")
check(c["resources"]["requests"] == {"cpu": "500m", "memory": "1k"}, f"GET: 0.5 → \"500m\", 1000 → \"1k\" ({c['resources']['requests']})")
check(c["ports"][0]["containerPort"] == 8080 and got["spec"]["terminationGracePeriodSeconds"] == 7,
      "non-quantity numbers stay numbers")

yaml = """apiVersion: v1
kind: Pod
metadata:
  name: q2
spec:
  containers:
  - name: c
    image: busybox
    resources:
      limits:
        cpu: 2
        memory: 1Gi
"""
code, out = req("POST", P, yaml, "application/yaml")
check(code == 201 and out["spec"]["containers"][0]["resources"]["limits"]["cpu"] == "2",
      f"YAML body `cpu: 2` stored as \"2\" ({code})")

code, out = req("PATCH", f"{P}/q1", {"metadata": {"labels": {"x": "y"}},
    "spec": {"activeDeadlineSeconds": 600}}, "application/merge-patch+json")
check(code == 200 and numbers_at_quantities(out) == [], f"merge PATCH answer has no numeric quantity ({code})")

_, lst = req("GET", P)
bad = [p for item in lst["items"] for p in numbers_at_quantities(item)]
check(bad == [], f"LIST: every quantity is a string ({bad})")

code, out = req("POST", "/api/v1/namespaces/default/limitranges", {"apiVersion": "v1", "kind": "LimitRange",
    "metadata": {"name": "lr"}, "spec": {"limits": [{"type": "Container", "max": {"cpu": 4}, "default": {"cpu": 1}}]}})
item = out["spec"]["limits"][0] if code == 201 else {}
check(code == 201 and item["max"]["cpu"] == "4" and item["default"]["cpu"] == "1", f"LimitRange max/default as strings ({code} {item})")
code, out = req("POST", "/api/v1/namespaces/default/resourcequotas", {"apiVersion": "v1", "kind": "ResourceQuota",
    "metadata": {"name": "rq"}, "spec": {"hard": {"pods": 10, "requests.cpu": 8}}})
check(code == 201 and out["spec"]["hard"] == {"pods": "10", "requests.cpu": "8"}, f"ResourceQuota hard as strings ({code})")
for path in ("/api/v1/namespaces/default/limitranges/lr", "/api/v1/namespaces/default/resourcequotas/rq",
             f"{P}/q1", f"{P}/q2"):
    req("DELETE", path)
sys.exit(failed)
PY
report
