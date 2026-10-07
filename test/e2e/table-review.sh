#!/usr/bin/env bash
#
# A Table request to a resource with no Table form is 406 (#126), the
# conformance spec "should return a 406 for a backend which does not
# implement metadata". A real apiserver on fastetcd.
#
# - SelfSubjectAccessReview, SubjectAccessReview, TokenReview POSTed with
#   only `application/json;as=Table;v=v1;g=meta.k8s.io`: 406 NotAcceptable
# - the same with kubectl's JSON fallback after the Table entries: answered
# - a LIST of pods asking for a Table still gets one
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
TABLE = "application/json;as=Table;v=v1;g=meta.k8s.io"
KUBECTL = TABLE + ",application/json;as=Table;v=v1beta1;g=meta.k8s.io,application/json"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body, accept):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": "application/json", "Accept": accept})
    r = c.getresponse(); raw = r.read(); c.close()
    try: return r.status, json.loads(raw)
    except ValueError: return r.status, raw
attrs = {"resourceAttributes": {"namespace": "default", "verb": "get", "resource": "pods"}}
reviews = {
    "selfsubjectaccessreviews": ("/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
        {"apiVersion": "authorization.k8s.io/v1", "kind": "SelfSubjectAccessReview", "spec": attrs}),
    "subjectaccessreviews": ("/apis/authorization.k8s.io/v1/subjectaccessreviews",
        {"apiVersion": "authorization.k8s.io/v1", "kind": "SubjectAccessReview", "spec": attrs | {"user": "someone"}}),
    "tokenreviews": ("/apis/authentication.k8s.io/v1/tokenreviews",
        {"apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview", "spec": {"token": "not-a-token"}}),
}
for name, (path, body) in reviews.items():
    code, out = req("POST", path, body, TABLE)
    check(code == 406 and isinstance(out, dict) and out.get("reason") == "NotAcceptable" and out.get("code") == 406,
          f"{name}: Table only → 406 NotAcceptable ({code} {out})")
    code, out = req("POST", path, body, KUBECTL)
    check(code in (200, 201) and isinstance(out, dict) and "status" in out, f"{name}: with a JSON fallback → answered ({code})")
code, out = req("GET", "/api/v1/namespaces/kube-system/pods", None, TABLE)
check(code == 200 and isinstance(out, dict) and out.get("kind") == "Table", f"pods LIST still a Table ({code} {out.get('kind') if isinstance(out, dict) else out})")
sys.exit(failed)
PY
report
