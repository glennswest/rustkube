#!/usr/bin/env bash
#
# Field validation of built-in objects and YAML bodies (#122), with the
# conformance suite's FieldValidation bodies, on a real apiserver on fastetcd.
#
# - Strict POST of a Deployment with an unknown and a duplicate field: 400
#   'strict decoding error: unknown field "spec.unknownField", duplicate field
#   "spec.replicas"'; with unknown metadata: 'unknown field
#   "metadata.unknownMeta"'
# - Warn (the default): 201 and a Warning header naming the field; Ignore:
#   201, no warning
# - a valid Deployment under Strict, and a Pod with a 1.34 pod-level
#   `resources` (the descriptors are release-1.36): 201
# - a POST without a Content-Type is JSON: 201
# - a YAML body (application/yaml) is accepted; under Strict a repeated YAML
#   key is 400 'line N: key "x" already set in map'
#
# Exit status is the number of failed checks.
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
def req(method, path, body, ctype="application/json"):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    h = {"Authorization": "Bearer " + os.environ["ADMIN"]}
    if ctype: h["Content-Type"] = ctype
    c.request(method, path, body=body if isinstance(body, bytes) else body.encode(), headers=h)
    r = c.getresponse(); raw = r.read(); warns = [v for k, v in r.getheaders() if k.lower() == "warning"]; c.close()
    try: out = json.loads(raw)
    except ValueError: out = raw.decode(errors="replace")
    return r.status, out, warns
D = "/apis/apps/v1/namespaces/default/deployments"
def dep(name, extra_spec="", extra_meta=""):
    return f'''{{"apiVersion": "apps/v1", "kind": "Deployment",
      "metadata": {{"name": "{name}", {extra_meta} "labels": {{"app": "nginx"}}}},
      "spec": {{ {extra_spec} "selector": {{"matchLabels": {{"app": "nginx"}}}},
        "template": {{"metadata": {{"labels": {{"app": "nginx"}}}},
          "spec": {{"containers": [{{"name": "nginx", "image": "nginx:latest"}}]}}}}}}}}'''

code, out, _ = req("POST", D + "?fieldManager=field_validation_mgr&fieldValidation=Strict",
                   dep("my-dep", '"unknownField": "foo", "replicas": 2, "replicas": 3,'))
check(code == 400 and 'strict decoding error: unknown field "spec.unknownField", duplicate field "spec.replicas"' in json.dumps(out).replace('\\"', '"'),
      f"Strict: unknown + duplicate field refused as upstream words it ({code} {out if code != 201 else ''})")
code, out, _ = req("POST", D + "?fieldValidation=Strict", dep("my-dep", "", '"unknownMeta": "foo",'))
check(code == 400 and 'unknown field "metadata.unknownMeta"' in json.dumps(out).replace('\\"', '"'),
      f"Strict: unknown metadata refused ({code})")
code, out, warns = req("POST", D, dep("warned", '"unknownField": "foo",'))
check(code == 201 and any("spec.unknownField" in w for w in warns), f"Warn (default): created, Warning header ({code} {warns})")
code, out, warns = req("POST", D + "?fieldValidation=Ignore", dep("ignored", '"unknownField": "foo",'))
check(code == 201 and not warns, f"Ignore: created, no warning ({code} {warns})")
code, out, _ = req("POST", D + "?fieldValidation=Strict", dep("valid"))
check(code == 201, f"Strict: a valid Deployment is created ({code} {out if code != 201 else ''})")
code, out, _ = req("POST", "/api/v1/namespaces/default/pods?fieldValidation=Strict", json.dumps({
    "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "podres"},
    "spec": {"resources": {"limits": {"cpu": "1"}}, "containers": [{"name": "c", "image": "i"}]}}))
check(code == 201, f"Strict: a 1.34 pod-level resources field is known ({code} {out if code != 201 else ''})")
code, out, _ = req("POST", D, dep("noctype"), ctype=None)
check(code == 201, f"no Content-Type: read as JSON ({code} {out if code != 201 else ''})")
yaml = """apiVersion: v1
kind: ConfigMap
metadata:
  name: from-yaml
data:
  a: "1"
"""
code, out, _ = req("POST", "/api/v1/namespaces/default/configmaps", yaml, "application/yaml")
check(code == 201 and out.get("data", {}).get("a") == "1", f"a YAML body is accepted ({code})")
dup = """apiVersion: v1
kind: ConfigMap
metadata:
  name: dup-yaml
data:
  a: "1"
  a: "2"
"""
code, out, _ = req("POST", "/api/v1/namespaces/default/configmaps?fieldValidation=Strict", dup, "application/yaml")
check(code == 400 and 'key \\"a\\" already set in map' in json.dumps(out) or (code == 400 and 'key "a" already set in map' in str(out)),
      f"Strict: a repeated YAML key is refused ({code} {out})")
sys.exit(failed)
PY
report
