#!/usr/bin/env bash
#
# An unserved resource is 404 (#110), against a real apiserver on fastetcd.
#
# - GET, POST, PUT, DELETE of a resource no built-in group-version serves
#   (`/api/v1/replicationcontrollerz`, `/apis/apps/v1/namespaces/default/
#   widgets`): 404 NotFound "the server could not find the requested
#   resource", and nothing is stored
# - served resources keep working: replicationcontrollers, resourcequotas
#   and podtemplates list with their own kinds; namespaces/finalize and
#   pods/log paths still reach their handlers
# - a CRD in a built-in group (gateway.networking.k8s.io grpcroutes) is
#   served once registered
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
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
notfound = lambda c, o: c == 404 and isinstance(o, dict) and o.get("reason") == "NotFound" and "could not find the requested resource" in o.get("message", "")
for path in ["/api/v1/replicationcontrollerz", "/api/v1/namespaces/default/replicationcontrollerz",
             "/apis/apps/v1/namespaces/default/widgets", "/apis/rbac.authorization.k8s.io/v1/nothings"]:
    code, out = req("GET", path)
    check(notfound(code, out), f"GET {path}: 404 ({code} {out.get('kind') if isinstance(out, dict) else ''})")
    code, out = req("POST", path, {"apiVersion": "v1", "kind": "Thing", "metadata": {"name": "x"}})
    check(notfound(code, out), f"POST {path}: 404 ({code})")
    code, _ = req("PUT", path + "/x", {"apiVersion": "v1", "kind": "Thing", "metadata": {"name": "x"}})
    check(code == 404, f"PUT {path}/x: 404 ({code})")
    check(req("GET", path + "/x")[0] == 404, f"…nothing stored under {path}")
for res, kind in [("replicationcontrollers", "ReplicationControllerList"), ("resourcequotas", "ResourceQuotaList"),
                  ("podtemplates", "PodTemplateList")]:
    code, out = req("GET", f"/api/v1/namespaces/default/{res}")
    check(code == 200 and out.get("kind") == kind, f"{res} lists as {kind} ({code} {out.get('kind')})")
req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "fin"}})
code, _ = req("GET", "/api/v1/namespaces/fin")
check(code == 200, f"a namespace by name still served ({code})")
code, out = req("GET", "/api/v1/namespaces/default/pods/nopod/log")
check(code == 404 and "could not find the requested resource" not in json.dumps(out), f"pods/log reaches its handler (its own 404: {code})")
req("POST", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions", {"apiVersion": "apiextensions.k8s.io/v1",
    "kind": "CustomResourceDefinition", "metadata": {"name": "grpcroutes.gateway.networking.k8s.io"},
    "spec": {"group": "gateway.networking.k8s.io", "scope": "Namespaced",
             "names": {"plural": "grpcroutes", "singular": "grpcroute", "kind": "GRPCRoute", "listKind": "GRPCRouteList"},
             "versions": [{"name": "v1", "served": True, "storage": True,
                           "schema": {"openAPIV3Schema": {"type": "object", "x-kubernetes-preserve-unknown-fields": True}}}]}})
ok = False
for _ in range(30):
    code, out = req("GET", "/apis/gateway.networking.k8s.io/v1/namespaces/default/grpcroutes")
    if code == 200: ok = True; break
    time.sleep(0.5)
check(ok, f"a CRD in a built-in group is served once registered ({code})")
sys.exit(failed)
PY
report
