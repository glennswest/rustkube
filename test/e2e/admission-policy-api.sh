#!/usr/bin/env bash
#
# The admission policy resources served (#119), what the conformance suite's
# four "…AdmissionPolicy(Binding) API operations" specs exercise. A real
# apiserver on fastetcd. Nothing evaluates the policies yet (#234).
#
# - discovery lists validatingadmissionpolicies (+ /status),
#   validatingadmissionpolicybindings, mutatingadmissionpolicies,
#   mutatingadmissionpolicybindings, cluster-scoped
# - each: create (with a Warning that it is not enforced), get, list,
#   merge patch, deletecollection by label; a protobuf GET answers protobuf
# - ValidatingAdmissionPolicy /status: a merge patch of status, spec kept
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
G = "/apis/admissionregistration.k8s.io/v1"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, ctype="application/json", accept="application/json"):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype, "Accept": accept})
    r = c.getresponse(); raw = r.read(); ct = r.getheader("Content-Type", "")
    warns = [v for k, v in r.getheaders() if k.lower() == "warning"]; c.close()
    try: out = json.loads(raw) if "json" in ct else raw
    except ValueError: out = raw
    return r.status, out, warns, ct

_, res, _, _ = req("GET", G)
names = {r["name"]: r for r in res["resources"]}
want = ["validatingadmissionpolicies", "validatingadmissionpolicies/status", "validatingadmissionpolicybindings",
        "mutatingadmissionpolicies", "mutatingadmissionpolicybindings"]
check(all(n in names and names[n]["namespaced"] is False for n in want), f"discovery lists the four, cluster-scoped ({sorted(names)})")

match = {"resourceRules": [{"apiGroups": ["apps"], "apiVersions": ["v1"], "operations": ["CREATE", "UPDATE"], "resources": ["deployments"]}]}
objs = {
    "validatingadmissionpolicies": ("ValidatingAdmissionPolicy", {"matchConstraints": match, "failurePolicy": "Fail",
        "validations": [{"expression": "object.spec.replicas <= 100"}]}),
    "validatingadmissionpolicybindings": ("ValidatingAdmissionPolicyBinding", {"policyName": "p", "validationActions": ["Deny"]}),
    "mutatingadmissionpolicies": ("MutatingAdmissionPolicy", {"matchConstraints": match, "failurePolicy": "Fail",
        "reinvocationPolicy": "Never",
        "mutations": [{"patchType": "ApplyConfiguration", "applyConfiguration": {"expression": "Object{spec: Object.spec{replicas: 100}}"}}]}),
    "mutatingadmissionpolicybindings": ("MutatingAdmissionPolicyBinding", {"policyName": "p"}),
}
for res, (kind, spec) in objs.items():
    for i in (1, 2):
        code, out, warns, _ = req("POST", f"{G}/{res}", {"apiVersion": "admissionregistration.k8s.io/v1", "kind": kind,
                                  "metadata": {"name": f"e2e-{i}", "labels": {"e2e": "api"}}, "spec": spec})
        if i == 1:
            check(code == 201 and out["kind"] == kind, f"{res}: create ({code})")
            check(any("not enforced" in w and "#234" in w for w in warns), f"{res}: a Warning says it is not enforced ({warns})")
    code, out, _, _ = req("GET", f"{G}/{res}/e2e-1")
    check(code == 200 and out["spec"] == spec | out["spec"], f"{res}: get ({code})")
    code, out, _, _ = req("GET", f"{G}/{res}?labelSelector=e2e%3Dapi")
    check(code == 200 and out["kind"] == kind + "List" and len(out["items"]) == 2, f"{res}: list ({code} {len(out.get('items', []))})")
    code, out, _, _ = req("PATCH", f"{G}/{res}/e2e-1", {"metadata": {"annotations": {"patched": "true"}}}, "application/merge-patch+json")
    check(code == 200 and out["metadata"]["annotations"]["patched"] == "true", f"{res}: merge patch ({code})")
    code, out, _, ct = req("GET", f"{G}/{res}/e2e-1", accept="application/vnd.kubernetes.protobuf")
    check(code == 200 and "protobuf" in ct, f"{res}: protobuf GET ({code} {ct})")
    if res == "validatingadmissionpolicies":
        code, out, _, _ = req("PATCH", f"{G}/{res}/e2e-1/status",
                              {"status": {"conditions": [{"type": "StatusUpdated", "status": "True", "reason": "E2E",
                                                          "message": "set", "lastTransitionTime": "2026-10-07T00:00:00Z"}]}},
                              "application/merge-patch+json")
        check(code == 200 and out["status"]["conditions"][0]["reason"] == "E2E" and out["spec"]["validations"],
              f"{res}: /status merge patch, spec kept ({code})")
    code, _, _, _ = req("DELETE", f"{G}/{res}?labelSelector=e2e%3Dapi")
    code2, out, _, _ = req("GET", f"{G}/{res}?labelSelector=e2e%3Dapi")
    check(code == 200 and len(out["items"]) == 0, f"{res}: deletecollection ({code}, {len(out['items'])} left)")
sys.exit(failed)
PY
report
