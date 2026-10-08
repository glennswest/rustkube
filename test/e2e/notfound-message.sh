#!/usr/bin/env bash
#
# NotFound names the object as the API does (#109), against a real apiserver
# on fastetcd: `namespaces "x" not found`, `deployments.apps "web" not
# found`, a custom resource's `widgets.<group>`, with Status `details`
# {name, group, kind} — never the storage key. An unregistered custom
# resource type is "the server could not find the requested resource".
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
for path, msg, details in [
    ("/api/v1/namespaces/openshift-config-managed", 'namespaces "openshift-config-managed" not found',
     {"name": "openshift-config-managed", "group": "", "kind": "namespaces"}),
    ("/api/v1/namespaces/default/pods/nope", 'pods "nope" not found', {"name": "nope", "group": "", "kind": "pods"}),
    ("/apis/apps/v1/namespaces/default/deployments/web", 'deployments.apps "web" not found', {"name": "web", "group": "apps", "kind": "deployments"}),
]:
    code, out = req("GET", path)
    check(code == 404 and out.get("message") == msg and out.get("details") == details and "/registry" not in json.dumps(out),
          f"GET {path}: {msg!r} ({code} {out.get('message')} {out.get('details')})")
req("POST", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions", {"apiVersion": "apiextensions.k8s.io/v1",
    "kind": "CustomResourceDefinition", "metadata": {"name": "widgets.nf.example.com"},
    "spec": {"group": "nf.example.com", "scope": "Namespaced", "names": {"plural": "widgets", "singular": "widget", "kind": "Widget"},
             "versions": [{"name": "v1", "served": True, "storage": True,
                           "schema": {"openAPIV3Schema": {"type": "object", "x-kubernetes-preserve-unknown-fields": True}}}]}})
for _ in range(30):
    if req("GET", "/apis/nf.example.com/v1/namespaces/default/widgets")[0] == 200: break
    time.sleep(0.3)
code, out = req("GET", "/apis/nf.example.com/v1/namespaces/default/widgets/w")
check(code == 404 and out.get("message") == 'widgets.nf.example.com "w" not found', f"a missing custom resource ({out.get('message')})")
code, out = req("GET", "/apis/nf.example.com/v1/namespaces/default/gadgets/g")
check(code == 404 and out.get("message") == "the server could not find the requested resource", f"an unregistered CR type ({out.get('message')})")
sys.exit(failed)
PY
report
