#!/usr/bin/env bash
#
# Custom resources under their CRD's structural schema (#121), over HTTP, with
# the conformance suite's own bodies (field_validation.go,
# custom_resource_definition.go). A real apiserver on fastetcd.
#
# - server-side apply, fieldValidation=Strict:
#   an undeclared root field → 400 ".unknownField: field not declared in schema";
#   unknown metadata at the root and in an x-kubernetes-embedded-resource →
#   ".metadata.unknownMeta" / ".spec.template.metadata.unknownSubMeta";
#   a repeated key under x-kubernetes-preserve-unknown-fields →
#   'line 9: key "foo" already set in map'; a valid CR is created
# - without Strict (Warn, the default): the CR is created, the undeclared
#   fields pruned, and each named in a Warning header
# - a JSON create with fieldValidation=Strict and a repeated key → 400
#   'duplicate field "spec.foo"'
# - defaulting: a default fills an absent field on create; a default added to
#   the CRD afterwards shows on a GET and a LIST of the stored object
# - validation (the flowsdn audit's C3/C4/C6), on a Namespaced and a Cluster
#   CRD: a missing required spec / field (C3) is 422 and not stored; an
#   omitted enum field is defaulted (C4); a repeated key in an
#   x-kubernetes-list-type=map list (C6) is 422 through create, PUT, merge
#   and JSON patch and apply, the stored object unchanged; bounds and enum
#   422 likewise; a valid patch is stored. Ratcheting: after the CRD
#   tightens, an object keeps its old value through an update of something
#   else, and cannot change it to another invalid one
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
def req(method, path, body=None, ctype="application/json"):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    data = body if isinstance(body, (bytes, str)) or body is None else json.dumps(body)
    conn.request(method, path, body=data, headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype})
    r = conn.getresponse(); raw = r.read(); hdrs = r.getheaders(); conn.close()
    try: out = json.loads(raw) if raw else None
    except ValueError: out = raw.decode(errors="replace")
    return r.status, out, [v for k, v in hdrs if k.lower() == "warning"]

PORTS = {"type": "array", "x-kubernetes-list-map-keys": ["containerPort", "protocol"], "x-kubernetes-list-type": "map",
         "items": {"type": "object", "required": ["containerPort", "protocol"], "properties": {
             "containerPort": {"type": "integer", "format": "int32"}, "hostIP": {"type": "string"},
             "hostPort": {"type": "integer", "format": "int32"}, "name": {"type": "string"}, "protocol": {"type": "string"}}}}
def crd(plural, spec_schema, scope="Cluster", required=None):
    kind = plural.capitalize()[:-1]
    code, out, _ = req("POST", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions", {
        "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
        "metadata": {"name": f"{plural}.schema.example.com"},
        "spec": {"group": "schema.example.com", "scope": scope,
                 "names": {"plural": plural, "singular": plural[:-1], "kind": kind, "listKind": kind + "List"},
                 "versions": [{"name": "v1", "served": True, "storage": True,
                               "schema": {"openAPIV3Schema": dict({"type": "object", "properties": {"spec": spec_schema}},
                                                                  **({"required": required} if required else {}))}}]}})
    if code >= 300: print(f"setup {plural}: {code} {out}"); sys.exit(100)
    for _ in range(40):
        if req("GET", f"/apis/schema.example.com/v1/{plural}")[0] == 200: break
        time.sleep(0.5)
    return kind
def apply(plural, name, yaml, strict=True):
    q = f"fieldManager=field_validation_mgr" + ("&fieldValidation=Strict" if strict else "")
    return req("PATCH", f"/apis/schema.example.com/v1/{plural}/{name}?{q}", yaml, "application/apply-patch+yaml")

# --- validation schema, unknown root field ------------------------------------
kind = crd("noxus", {"type": "object", "properties": {"foo": {"type": "string"}, "cronSpec": {"type": "string"}, "ports": PORTS}})
body = lambda name, extra="": f"""
apiVersion: schema.example.com/v1
kind: {kind}
metadata:
  name: {name}
{extra}spec:
  foo: foo1
  cronSpec: "* * * * */5"
  ports:
  - name: x
    containerPort: 80
    protocol: TCP"""
code, out, _ = apply("noxus", "valid", body("valid"))
check(code in (200, 201), f"Strict apply of a valid CR succeeds ({code} {out if code >= 300 else ''})")
code, out, _ = apply("noxus", "mytest", body("mytest", "unknownField: unknown\n"))
check(code == 400 and ".unknownField: field not declared in schema" in json.dumps(out),
      f"Strict apply with an undeclared field: 400 '.unknownField: field not declared in schema' ({code} {out})")
code, out, warns = apply("noxus", "warned", body("warned", "unknownField: unknown\n"), strict=False)
check(code in (200, 201) and "unknownField" not in out and any("unknownField" in w for w in warns),
      f"without Strict: created, pruned, and warned ({code} {warns})")
code, out, _ = req("POST", "/apis/schema.example.com/v1/noxus?fieldValidation=Strict",
                   b'{"apiVersion":"schema.example.com/v1","kind":"%s","metadata":{"name":"dupjson"},"spec":{"foo":"a","foo":"b"}}' % kind.encode())
check(code == 400 and 'duplicate field \\"spec.foo\\"' in json.dumps(out) or (code == 400 and 'duplicate field "spec.foo"' in str(out)),
      f"Strict JSON create with a repeated key: 400 duplicate field (code {code} {out})")

# --- unknown metadata, root and embedded --------------------------------------
kind = crd("embeds", {"type": "object", "x-kubernetes-preserve-unknown-fields": True, "properties": {
    "template": {"type": "object", "x-kubernetes-embedded-resource": True, "x-kubernetes-preserve-unknown-fields": True,
                 "properties": {"spec": {"type": "object", "x-kubernetes-preserve-unknown-fields": True}}}}})
yaml = f"""
apiVersion: schema.example.com/v1
kind: {kind}
metadata:
  name: mytest
  unknownMeta: unknown
spec:
  template:
    apiVersion: v1
    kind: Pod
    metadata:
        unknownSubMeta: unknown
        name: x
    spec: {{}}"""
code, out, _ = apply("embeds", "mytest", yaml)
s = json.dumps(out)
check(code == 400 and (".spec.template.metadata.unknownSubMeta: field not declared in schema" in s
                       or ".metadata.unknownMeta: field not declared in schema" in s),
      f"Strict apply with unknown metadata (root, embedded): 400 ({code} {out})")

# --- duplicates under preserve-unknown-fields ---------------------------------
kind = crd("dups", {"type": "object", "x-kubernetes-preserve-unknown-fields": True,
                    "properties": {"foo": {"type": "string"}, "cronSpec": {"type": "string"}, "ports": PORTS}})
yaml = f"""
apiVersion: schema.example.com/v1
kind: {kind}
metadata:
  name: mytest
spec:
  unknown: uk1
  foo: foo1
  foo: foo2
  cronSpec: "* * * * */5"
  ports:
  - name: x
    containerPort: 80
    protocol: TCP"""
code, out, _ = apply("dups", "mytest", yaml)
check(code == 400 and 'line 9: key \\"foo\\" already set in map' in json.dumps(out),
      f"Strict apply with a repeated key: 400 'line 9: key \"foo\" already set in map' ({code} {out})")

# --- defaulting on request and from storage -----------------------------------
crd("defs", {"type": "object", "properties": {"a": {"type": "string", "default": "A"}}})
code, out, _ = req("POST", "/apis/schema.example.com/v1/defs", {"apiVersion": "schema.example.com/v1", "kind": "Def",
                                                                 "metadata": {"name": "d1"}, "spec": {}})
check(code == 201 and out["spec"].get("a") == "A", f"a default fills an absent field on create ({out.get('spec') if isinstance(out, dict) else out})")
_, c, _ = req("GET", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/defs.schema.example.com")
c["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]["b"] = {"type": "string", "default": "B"}
code, _, _ = req("PUT", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/defs.schema.example.com", c)
got = None
for _ in range(40):
    _, got, _ = req("GET", "/apis/schema.example.com/v1/defs/d1")
    if got["spec"].get("b") == "B": break
    time.sleep(0.5)
check(got["spec"].get("b") == "B" and got["spec"].get("a") == "A", f"a default added to the CRD later shows on GET ({got['spec']})")
_, lst, _ = req("GET", "/apis/schema.example.com/v1/defs")
check(lst["items"][0]["spec"].get("b") == "B", "…and on LIST")

# --- validation: the flowsdn audit's C3 (required), C4 (default), C6 (keyed list)
POLICY = {"type": "object", "required": ["selector"], "properties": {
    "selector": {"type": "string", "pattern": "^[a-z]+$"},
    "family": {"type": "string", "enum": ["IPv4", "IPv6"], "default": "IPv4"},
    "priority": {"type": "integer", "minimum": 1, "maximum": 10},
    "rules": {"type": "array", "x-kubernetes-list-type": "map", "x-kubernetes-list-map-keys": ["name", "protocol"],
              "items": {"type": "object", "required": ["name"],
                        "properties": {"name": {"type": "string"}, "protocol": {"type": "string", "default": "TCP"}}}}}}
MERGE, JSONP, APPLY = "application/merge-patch+json", "application/json-patch+json", "application/apply-patch+yaml"
for scope, plural in (("Namespaced", "nspolicies"), ("Cluster", "policies")):
    kind = crd(plural, POLICY, scope, required=["spec"])
    base = f"/apis/schema.example.com/v1/namespaces/default/{plural}" if scope == "Namespaced" else f"/apis/schema.example.com/v1/{plural}"
    obj = lambda name, spec=None: dict({"apiVersion": "schema.example.com/v1", "kind": kind, "metadata": {"name": name}},
                                       **({"spec": spec} if spec is not None else {}))
    invalid = lambda code, out, field: code == 422 and f'{kind}.schema.example.com' in json.dumps(out) and field in json.dumps(out)
    gone = lambda name: req("GET", f"{base}/{name}")[0] == 404
    code, out, _ = req("POST", base, obj("nospec"))
    check(invalid(code, out, "spec: Required value") and gone("nospec"), f"{scope} C3: no spec → 422 'spec: Required value', not stored ({code} {out})")
    code, out, _ = req("POST", base, obj("nosel", {"family": "IPv6"}))
    check(invalid(code, out, "spec.selector: Required value") and gone("nosel"), f"{scope} C3: no spec.selector → 422, not stored ({code} {out})")
    code, out, _ = req("POST", base, obj("p", {"selector": "web", "rules": [{"name": "a"}]}))
    check(code == 201 and out["spec"].get("family") == "IPv4" and out["spec"]["rules"] == [{"name": "a", "protocol": "TCP"}],
          f"{scope} C4: the omitted family and protocol are defaulted ({code} {out.get('spec') if isinstance(out, dict) else out})")
    code, out, _ = req("POST", base, obj("dup", {"selector": "web", "rules": [{"name": "a"}, {"name": "a", "protocol": "TCP"}]}))
    check(invalid(code, out, "spec.rules[1]: Duplicate value") and gone("dup"), f"{scope} C6: a repeated rule key on create → 422, not stored ({code} {out})")
    _, stored, _ = req("GET", f"{base}/p")
    rv = stored["metadata"]["resourceVersion"]
    put = json.loads(json.dumps(stored)); put["spec"]["rules"].append({"name": "a", "protocol": "TCP"})
    code, out, _ = req("PUT", f"{base}/p", put)
    check(invalid(code, out, "spec.rules[1]: Duplicate value"), f"{scope} C6: through PUT → 422 ({code} {out})")
    code, out, _ = req("PATCH", f"{base}/p", {"spec": {"rules": [{"name": "a"}, {"name": "a"}]}}, MERGE)
    check(invalid(code, out, "spec.rules[1]: Duplicate value"), f"{scope} C6: through a merge patch → 422 ({code} {out})")
    code, out, _ = req("PATCH", f"{base}/p", [{"op": "add", "path": "/spec/rules/-", "value": {"name": "a"}}], JSONP)
    check(invalid(code, out, "spec.rules[1]: Duplicate value"), f"{scope} C6: through a JSON patch → 422 ({code} {out})")
    ns = "\n  namespace: default" if scope == "Namespaced" else ""
    yaml = f"apiVersion: schema.example.com/v1\nkind: {kind}\nmetadata:\n  name: p{ns}\nspec:\n  selector: web\n  rules:\n  - name: a\n  - name: a\n"
    code, out, _ = req("PATCH", f"{base}/p?fieldManager=rig&force=true", yaml, APPLY)
    check(invalid(code, out, "Duplicate value"), f"{scope} C6: through server-side apply → 422 ({code} {out})")
    code, out, _ = req("PATCH", f"{base}/p", {"spec": {"priority": 11, "family": "IPX"}}, MERGE)
    check(invalid(code, out, "spec.priority: Invalid value: 11") and "spec.family: Unsupported value" in json.dumps(out),
          f"{scope}: bounds and enum → 422 with both errors ({code} {out})")
    _, now, _ = req("GET", f"{base}/p")
    check(now["metadata"]["resourceVersion"] == rv and now["spec"] == stored["spec"],
          f"{scope}: every refused update left the stored object as it was ({now['spec']})")
    code, out, _ = req("PATCH", f"{base}/p", {"spec": {"priority": 3, "rules": [{"name": "a"}, {"name": "a", "protocol": "UDP"}]}}, MERGE)
    check(code == 200 and out["spec"]["priority"] == 3 and len(out["spec"]["rules"]) == 2,
          f"{scope}: a valid patch (same name, other protocol) is stored ({code} {out.get('spec') if isinstance(out, dict) else out})")

# --- ratcheting: the CRD tightens under a stored object -------------------------
_, c, _ = req("GET", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/policies.schema.example.com")
c["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]["selector"]["maxLength"] = 2
code, _, _ = req("PUT", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/policies.schema.example.com", c)
# In effect once a fresh create with a 3-letter selector is refused.
for i in range(40):
    code, out, _ = req("POST", "/apis/schema.example.com/v1/policies",
                       {"apiVersion": "schema.example.com/v1", "kind": "Policie", "metadata": {"name": f"probe{i}"}, "spec": {"selector": "abc"}})
    if code == 422: break
    time.sleep(0.5)
check(code == 422 and "spec.selector: Too long" in json.dumps(out), f"the tightened CRD (maxLength 2) refuses a new 3-letter selector ({code} {out})")
code, out, _ = req("PATCH", "/apis/schema.example.com/v1/policies/p", {"spec": {"selector": "longer"}}, MERGE)
check(code == 422 and "spec.selector: Too long" in json.dumps(out), f"ratcheting: changing it to another over-long value is 422 ({code} {out})")
code, out, _ = req("PATCH", "/apis/schema.example.com/v1/policies/p", {"spec": {"priority": 4}}, MERGE)
check(code == 200 and out["spec"]["selector"] == "web" and out["spec"]["priority"] == 4,
      f"ratcheting: an update of another field keeps the now-too-long 'web' ({code} {out.get('spec') if isinstance(out, dict) else out})")
sys.exit(failed)
PY
report
