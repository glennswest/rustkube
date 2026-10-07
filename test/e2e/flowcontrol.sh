#!/usr/bin/env bash
#
# flowcontrol.apiserver.k8s.io/v1 served (#118), what the conformance suite's
# "API priority and fairness should support FlowSchema / PriorityLevel-
# Configuration API operations" specs exercise. A real apiserver on fastetcd.
# Nothing enforces the objects.
#
# - /apis lists the group; discovery lists flowschemas,
#   prioritylevelconfigurations and their /status, cluster-scoped
# - upstream's mandatory exempt and catch-all objects are bootstrapped
# - each resource: create, get, list by label, a watch delivers ADDED, merge
#   patch, PUT, /status merge patch (spec kept) and GET, protobuf GET,
#   delete, deletecollection
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, socket, ssl, sys, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
G = "/apis/flowcontrol.apiserver.k8s.io/v1"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, ctype="application/json", accept="application/json"):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype, "Accept": accept})
    r = c.getresponse(); raw = r.read(); ct = r.getheader("Content-Type", ""); c.close()
    try: out = json.loads(raw) if "json" in ct else raw
    except ValueError: out = raw
    return r.status, out, ct

_, groups, _ = req("GET", "/apis")
check(any(g["name"] == "flowcontrol.apiserver.k8s.io" for g in groups["groups"]), "/apis lists flowcontrol.apiserver.k8s.io")
_, res, _ = req("GET", G)
names = {r["name"]: r for r in res["resources"]}
want = ["flowschemas", "flowschemas/status", "prioritylevelconfigurations", "prioritylevelconfigurations/status"]
check(all(n in names and names[n]["namespaced"] is False for n in want), f"discovery lists the four, cluster-scoped ({sorted(names)})")
for res, name in [("prioritylevelconfigurations", "exempt"), ("prioritylevelconfigurations", "catch-all"),
                  ("flowschemas", "exempt"), ("flowschemas", "catch-all")]:
    code, out, _ = req("GET", f"{G}/{res}/{name}")
    check(code == 200, f"mandatory {res}/{name} bootstrapped ({code})")
code, out, _ = req("GET", f"{G}/flowschemas/catch-all")
check(code == 200 and out["spec"]["matchingPrecedence"] == 10000 and out["spec"]["priorityLevelConfiguration"]["name"] == "catch-all",
      "catch-all FlowSchema has upstream's spec")

def watch_added(path, name):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=15)
    c.request("GET", path + "?watch=true&labelSelector=e2e%3Dapf", headers={"Authorization": "Bearer " + os.environ["ADMIN"]})
    r = c.getresponse()
    try:
        while True:
            line = r.fp.readline()
            if not line: return False
            ev = json.loads(line)
            if ev["type"] == "ADDED" and ev["object"]["metadata"]["name"] == name: return True
    except (socket.timeout, ValueError):
        return False
    finally:
        c.close()

objs = {
    "flowschemas": ("FlowSchema", {"matchingPrecedence": 500, "priorityLevelConfiguration": {"name": "e2e-1"},
        "distinguisherMethod": {"type": "ByUser"},
        "rules": [{"subjects": [{"kind": "User", "user": {"name": "e2e"}}],
                   "nonResourceRules": [{"verbs": ["*"], "nonResourceURLs": ["*"]}]}]}),
    "prioritylevelconfigurations": ("PriorityLevelConfiguration", {"type": "Limited", "limited": {
        "nominalConcurrencyShares": 2, "lendablePercent": 0,
        "limitResponse": {"type": "Queue", "queuing": {"queues": 16, "handSize": 4, "queueLengthLimit": 50}}}}),
}
for res, (kind, spec) in objs.items():
    for i in (1, 2):
        code, out, _ = req("POST", f"{G}/{res}", {"apiVersion": "flowcontrol.apiserver.k8s.io/v1", "kind": kind,
                              "metadata": {"name": f"e2e-{i}", "labels": {"e2e": "apf"}}, "spec": spec})
        if i == 1:
            check(code == 201 and out["kind"] == kind, f"{res}: create ({code})")
    check(watch_added(f"{G}/{res}", "e2e-1"), f"{res}: watch delivers ADDED")
    code, out, _ = req("GET", f"{G}/{res}/e2e-1")
    check(code == 200 and out["spec"] == spec | out["spec"], f"{res}: get ({code})")
    code, out, _ = req("GET", f"{G}/{res}?labelSelector=e2e%3Dapf")
    check(code == 200 and out["kind"] == kind + "List" and len(out["items"]) == 2, f"{res}: list ({code} {len(out.get('items', []))})")
    code, out, _ = req("PATCH", f"{G}/{res}/e2e-1", {"metadata": {"annotations": {"patched": "true"}}}, "application/merge-patch+json")
    check(code == 200 and out["metadata"]["annotations"]["patched"] == "true", f"{res}: merge patch ({code})")
    out["metadata"]["labels"]["updated"] = "true"
    code, out, _ = req("PUT", f"{G}/{res}/e2e-1", out)
    check(code == 200 and out["metadata"]["labels"]["updated"] == "true", f"{res}: update ({code})")
    code, out, _ = req("PATCH", f"{G}/{res}/e2e-1/status",
                       {"status": {"conditions": [{"type": "StatusUpdated", "status": "True", "reason": "E2E",
                                                   "message": "set", "lastTransitionTime": "2026-10-07T00:00:00Z"}]}},
                       "application/merge-patch+json")
    check(code == 200 and out["status"]["conditions"][0]["reason"] == "E2E" and out["spec"] == spec | out["spec"],
          f"{res}: /status merge patch, spec kept ({code})")
    code, out, _ = req("GET", f"{G}/{res}/e2e-1/status")
    check(code == 200 and out["status"]["conditions"][0]["type"] == "StatusUpdated", f"{res}: /status get ({code})")
    code, out, ct = req("GET", f"{G}/{res}/e2e-1", accept="application/vnd.kubernetes.protobuf")
    check(code == 200 and "protobuf" in ct, f"{res}: protobuf GET ({code} {ct})")
    code, _, _ = req("DELETE", f"{G}/{res}/e2e-2")
    check(code == 200, f"{res}: delete ({code})")
    code, _, _ = req("DELETE", f"{G}/{res}?labelSelector=e2e%3Dapf")
    _, out, _ = req("GET", f"{G}/{res}?labelSelector=e2e%3Dapf")
    check(code == 200 and len(out["items"]) == 0, f"{res}: deletecollection ({code}, {len(out['items'])} left)")
sys.exit(failed)
PY
report
