#!/usr/bin/env bash
#
# CRD schemas published in /openapi/v2 and /openapi/v3 (#120), against a
# real apiserver on fastetcd — the CustomResourcePublishOpenAPI specs' checks.
#
# - a CRD with a schema: /openapi/v2 definition
#   `com.example.<group-rest>.v1.<Kind>` equal to the schema once apiVersion,
#   kind, metadata and x-kubernetes-group-version-kind are dropped
# - a CRD without a schema: a definition exists
# - /openapi/v3 lists `apis/<group>/<version>`; that document has the schema
#   (with its GVK) and the resource's paths
# - kubectl explain prints the CRD's description and fields (openapi v3)
# - two versions: one renamed → the new name published, the old gone; one
#   made unserved → its definition removed (and it is no longer served)
# - CRD deleted → its definitions gone
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
KUBECTL=${RK_TOOLS:+$RK_TOOLS/kubectl}
[ -x "${KUBECTL:-}" ] || KUBECTL=$(command -v kubectl || true)
cat >"$W/admin.kubeconfig" <<KC
apiVersion: v1
kind: Config
clusters: [ { name: rk, cluster: { server: "$API", insecure-skip-tls-verify: true } } ]
users: [ { name: u, user: { token: "$ADMIN" } } ]
contexts: [ { name: rk, context: { cluster: rk, user: u, namespace: default } } ]
current-context: rk
KC
export API ADMIN KUBECTL W
python3 - <<'PY' || FAIL=$?
import copy, http.client, json, os, re, ssl, subprocess, sys, time, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": "application/json", "Accept": "application/json"})
    r = c.getresponse(); raw = r.read(); c.close()
    try: return r.status, json.loads(raw)
    except ValueError: return r.status, raw
def until(f, secs=20):
    end = time.monotonic() + secs
    while time.monotonic() < end:
        v = f()
        if v: return v
        time.sleep(0.3)
    return f()
SCHEMA = {"description": "Foo CRD for Testing", "type": "object", "properties": {
    "spec": {"type": "object", "description": "Specification of Foo", "properties": {
        "bars": {"description": "List of Bars and their specs.", "type": "array", "items": {
            "type": "object", "required": ["name"], "properties": {
                "name": {"description": "Name of Bar.", "type": "string"},
                "age": {"description": "Age of Bar.", "type": "string"},
                "bazs": {"description": "List of Bazs.", "items": {"type": "string"}, "type": "array"}}}}}},
    "status": {"description": "Status of Foo", "type": "object", "properties": {
        "bars": {"description": "List of Bars and their statuses.", "type": "array", "items": {
            "type": "object", "properties": {"name": {"description": "Name of Bar.", "type": "string"},
                                             "available": {"description": "Whether the Bar is installed.", "type": "boolean"}}}}}}}}
def crd(group, kind, versions):
    plural = kind.lower() + "s"
    return {"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
            "metadata": {"name": f"{plural}.{group}"},
            "spec": {"group": group, "scope": "Namespaced",
                     "names": {"plural": plural, "singular": kind.lower(), "kind": kind, "listKind": kind + "List"},
                     "versions": versions}}
def v(name, schema=SCHEMA, served=True, storage=False):
    out = {"name": name, "served": served, "storage": storage}
    if schema is not None: out["schema"] = {"openAPIV3Schema": schema}
    return out
CRDS = "/apis/apiextensions.k8s.io/v1/customresourcedefinitions"
defs = lambda: req("GET", "/openapi/v2")[1]["definitions"]
def stripped(d):
    d = copy.deepcopy(d)
    for p in ("apiVersion", "kind", "metadata"): d.get("properties", {}).pop(p, None)
    d.pop("x-kubernetes-group-version-kind", None)
    return d

G = "crd-publish-openapi-test-foo.example.com"
code, _ = req("POST", CRDS, crd(G, "E2eTestFoo", [v("v1", storage=True)]))
name = "com.example.crd-publish-openapi-test-foo.v1.E2eTestFoo"
got = until(lambda: defs().get(name))
check(got is not None and stripped(got) == SCHEMA, f"v2 definition {name} is the schema ({code})")
check(got is not None and got.get("x-kubernetes-group-version-kind") == [{"group": G, "version": "v1", "kind": "E2eTestFoo"}], "…with its GVK")
G2 = "crd-publish-openapi-test-empty.example.com"
req("POST", CRDS, crd(G2, "E2eTestEmpty", [v("v1", schema=None, storage=True)]))
check(until(lambda: "com.example.crd-publish-openapi-test-empty.v1.E2eTestEmpty" in defs()), "a CRD without a schema has a definition")

idx = req("GET", "/openapi/v3")[1]["paths"]
check(f"apis/{G}/v1" in idx, "/openapi/v3 lists the CRD's group-version")
doc = req("GET", idx.get(f"apis/{G}/v1", {}).get("serverRelativeURL", "/openapi/v3/x"))[1]
s3 = doc.get("components", {}).get("schemas", {}).get(name, {})
check(stripped(s3) == SCHEMA and f"/apis/{G}/v1/namespaces/{{namespace}}/e2etestfoos" in doc.get("paths", {}),
      "the v3 document has the schema and the paths")
if os.environ.get("KUBECTL"):
    out = subprocess.run([os.environ["KUBECTL"], "--kubeconfig", os.environ["W"] + "/admin.kubeconfig", "explain", "e2etestfoos"],
                         capture_output=True, text=True).stdout
    check(re.search(r"(?s)DESCRIPTION:.*Foo CRD for Testing.*FIELDS:.*apiVersion.*<string>.*APIVersion defines.*spec.*<Object>.*Specification of Foo", out) is not None,
          f"kubectl explain shows the CRD ({out[:200]!r})")
    out = subprocess.run([os.environ["KUBECTL"], "--kubeconfig", os.environ["W"] + "/admin.kubeconfig", "explain", "e2etestfoos.spec.bars"],
                         capture_output=True, text=True).stdout
    check("List of Bars and their specs." in out and "Name of Bar." in out, f"kubectl explain e2etestfoos.spec.bars ({out[:160]!r})")
else:
    print("SKIP  kubectl explain (no kubectl)")

# Versions: rename one, unserve one.
G3 = "crd-publish-openapi-test-multi.example.com"
req("POST", CRDS, crd(G3, "E2eTestMulti", [v("v2", storage=True), v("v3")]))
n = lambda ver: f"com.example.crd-publish-openapi-test-multi.{ver}.E2eTestMulti"
check(until(lambda: n("v2") in defs() and n("v3") in defs()), "two versions published")
cur = req("GET", f"{CRDS}/e2etestmultis.{G3}")[1]
cur["spec"]["versions"][1]["name"] = "v4"
code, _ = req("PUT", f"{CRDS}/e2etestmultis.{G3}", cur)
check(until(lambda: n("v4") in defs() and n("v3") not in defs() and n("v2") in defs()), f"renamed v3 → v4: v4 published, v3 gone ({code})")
cur = req("GET", f"{CRDS}/e2etestmultis.{G3}")[1]
cur["spec"]["versions"][1]["served"] = False
req("PUT", f"{CRDS}/e2etestmultis.{G3}", cur)
check(until(lambda: n("v4") not in defs() and n("v2") in defs()), "v4 unserved: its definition removed")
check(until(lambda: req("GET", f"/apis/{G3}/v4/namespaces/default/e2etestmultis")[0] == 404), "…and v4 is no longer served")
req("DELETE", f"{CRDS}/e2etestfoos.{G}")
check(until(lambda: name not in defs()), "CRD deleted: its definition gone")
sys.exit(failed)
PY
report
