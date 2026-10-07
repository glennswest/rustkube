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
def crd(plural, spec_schema):
    kind = plural.capitalize()[:-1]
    code, out, _ = req("POST", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions", {
        "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
        "metadata": {"name": f"{plural}.schema.example.com"},
        "spec": {"group": "schema.example.com", "scope": "Cluster",
                 "names": {"plural": plural, "singular": plural[:-1], "kind": kind, "listKind": kind + "List"},
                 "versions": [{"name": "v1", "served": True, "storage": True,
                               "schema": {"openAPIV3Schema": {"type": "object", "properties": {"spec": spec_schema}}}}]}})
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
sys.exit(failed)
PY
report
