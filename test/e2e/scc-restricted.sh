#!/usr/bin/env bash
#
# restricted-v2 (#321): OpenShift manifests — no runAsUser, no fsGroup, no
# root — run unchanged, as a UID from the namespace's range. A real
# apiserver and controller-manager on fastetcd.
#
# - a Namespace gets openshift.io/sa.scc.uid-range and
#   sa.scc.supplemental-groups (N/10000), a different block per namespace
# - a Deployment's pod (created by the controller-manager) gets restricted-v2:
#   runAsUser = the range's first UID, runAsNonRoot, fsGroup, no privilege
#   escalation, drop ALL, seccomp RuntimeDefault, openshift.io/scc
# - alice (edit in the namespace) pinning UID 0 is refused in OpenShift's
#   words; a UID in the range is kept; a privileged container is refused
# - the admin's own pod is admitted under anyuid, unchanged; a pod whose
#   ServiceAccount is bound to system:openshift:scc:anyuid runs as UID 0
# - storm-* and an enforce=privileged namespace are not governed
# - alice's admitted pod cannot be updated around SCC (#359, review finding
#   198fa7c65883): a PUT making it privileged as UID 0, a merge patch turning
#   on hostNetwork, an ephemeral container added by PUT and the mirror-pod
#   annotation are 422 and leave it as it was; a new image is allowed
# - PodSecurity on updates (#361, review finding 6fcd683fb638): a hostNetwork
#   pod admitted while its namespace enforced `privileged` cannot take a new
#   image once the namespace enforces `baseline` (403, unchanged); adding a
#   toleration, which PodSecurity does not consider, still works
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
ALICE=$(token alice '["shop-devs"]')
export API ADMIN ALICE
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"])
CTX = ssl._create_unverified_context()
UID, SUP, SCC = "openshift.io/sa.scc.uid-range", "openshift.io/sa.scc.supplemental-groups", "openshift.io/scc"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(who, method, path, body=None, ctype="application/json"):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + os.environ[who], "Content-Type": ctype})
    r = conn.getresponse(); raw = r.read(); conn.close()
    try: return r.status, json.loads(raw)
    except ValueError: return r.status, raw

def setup(code_out, what):
    code, out = code_out
    if code >= 300 and code != 409:
        print(f"setup {what}: {code} {out}"); sys.exit(100)

def namespace(name, labels=None):
    setup(req("ADMIN", "POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace",
          "metadata": {"name": name, "labels": labels or {}}}), name)
    setup(req("ADMIN", "POST", f"/apis/rbac.authorization.k8s.io/v1/namespaces/{name}/rolebindings", {
        "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding", "metadata": {"name": "devs"},
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "edit"},
        "subjects": [{"kind": "Group", "name": "shop-devs", "apiGroup": "rbac.authorization.k8s.io"}]}), f"{name} devs")
    return req("ADMIN", "GET", f"/api/v1/namespaces/{name}")[1]

def pod(name, spec_extra=None, csc=None, sa=None):
    c = {"name": "app", "image": "registry.example/app:1"}
    if csc: c["securityContext"] = csc
    spec = {"containers": [c]}
    if sa: spec["serviceAccountName"] = sa
    spec.update(spec_extra or {})
    return {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name}, "spec": spec}

def start(ns):
    a = ns["metadata"].get("annotations", {})
    return int(a.get(UID, "0/0").split("/")[0])

shop = namespace("shop")
shop2 = namespace("shop2")
a, a2 = shop["metadata"].get("annotations", {}), shop2["metadata"].get("annotations", {})
check(a.get(UID, "").endswith("/10000") and a.get(SUP) == a.get(UID), f"shop has its ranges ({a.get(UID)!r}, {a.get(SUP)!r})")
check(start(shop) >= 1000000000 and start(shop2) != start(shop), f"shop2 has another block ({a2.get(UID)!r})")
first = start(shop)
check(req("ADMIN", "GET", "/apis/rbac.authorization.k8s.io/v1/clusterroles/system:openshift:scc:anyuid")[0] == 200,
      "ClusterRole system:openshift:scc:anyuid bootstrapped")

# A Deployment, as an OpenShift manifest writes it: nothing about users.
setup(req("ADMIN", "POST", "/apis/apps/v1/namespaces/shop/deployments", {
    "apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"},
    "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "web"}},
             "template": {"metadata": {"labels": {"app": "web"}},
                          "spec": {"containers": [{"name": "web", "image": "registry.example/web:1"}]}}}}), "deployment")
p = None
for _ in range(120):
    items = req("ADMIN", "GET", "/api/v1/namespaces/shop/pods?labelSelector=app%3Dweb")[1].get("items", [])
    if items: p = items[0]; break
    time.sleep(0.5)
check(p is not None, "the Deployment's pod was created")
if p:
    sc, psc = p["spec"]["containers"][0].get("securityContext", {}), p["spec"].get("securityContext", {})
    check(sc.get("runAsUser") == first, f"runAsUser is the range's first UID ({sc.get('runAsUser')} vs {first})")
    check(sc.get("runAsNonRoot") is True and sc.get("allowPrivilegeEscalation") is False
          and sc.get("capabilities", {}).get("drop") == ["ALL"], f"non-root, no escalation, drop ALL ({sc})")
    check(psc.get("fsGroup") == first and psc.get("seccompProfile", {}).get("type") == "RuntimeDefault",
          f"fsGroup from the range, seccomp RuntimeDefault ({psc})")
    check(p["metadata"].get("annotations", {}).get(SCC) == "restricted-v2", "annotated openshift.io/scc: restricted-v2")

code, out = req("ALICE", "POST", "/api/v1/namespaces/shop/pods", pod("root", csc={"runAsUser": 0}))
msg = out.get("message", "") if isinstance(out, dict) else str(out)
check(code == 403 and "unable to validate against any security context constraint" in msg
      and f"must be in the ranges: [{first}, {first + 9999}]" in msg, f"alice's UID 0 refused ({code} {msg})")
code, out = req("ALICE", "POST", "/api/v1/namespaces/shop/pods", pod("inrange", csc={"runAsUser": first + 5}))
check(code == 201 and out["spec"]["containers"][0]["securityContext"]["runAsUser"] == first + 5, f"a UID in the range is kept ({code})")

# Updates cannot undo what SCC admitted (#359, review finding 198fa7c65883).
IP = "/api/v1/namespaces/shop/pods/inrange"
admitted = req("ALICE", "GET", IP)[1]
def unchanged():
    now = req("ALICE", "GET", IP)[1]
    return now["spec"] == admitted["spec"] and now["metadata"].get("annotations") == admitted["metadata"].get("annotations")
escalated = json.loads(json.dumps(admitted))
escalated["spec"]["containers"][0]["securityContext"] = {"privileged": True, "runAsUser": 0}
code, out = req("ALICE", "PUT", IP, escalated)
check(code == 422 and "pod updates may not change fields" in str(out) and unchanged(), f"PUT to privileged UID 0: 422, unchanged ({code})")
code, out = req("ALICE", "PATCH", IP, {"spec": {"hostNetwork": True}}, "application/merge-patch+json")
check(code == 422 and unchanged(), f"merge patch hostNetwork: 422, unchanged ({code})")
debug = json.loads(json.dumps(admitted))
debug["spec"]["ephemeralContainers"] = [{"name": "debug", "image": "busybox", "securityContext": {"privileged": True}}]
code, out = req("ALICE", "PUT", IP, debug)
check(code == 422 and unchanged(), f"ephemeral container added by PUT: 422, unchanged ({code})")
code, out = req("ALICE", "PATCH", IP, {"metadata": {"annotations": {"kubernetes.io/config.mirror": "x"}}}, "application/merge-patch+json")
check(code == 422 and unchanged(), f"mirror-pod annotation added: 422, unchanged ({code})")
code, out = req("ALICE", "PATCH", IP, {"spec": {"containers": [{"name": "app", "image": "registry.example/app:2"}]}},
                "application/strategic-merge-patch+json")
check(code == 200 and out["spec"]["containers"][0]["image"] == "registry.example/app:2"
      and out["spec"]["containers"][0]["securityContext"] == admitted["spec"]["containers"][0]["securityContext"],
      f"a new image is allowed, its securityContext kept ({code})")
code, out = req("ALICE", "POST", "/api/v1/namespaces/shop/pods", pod("priv", csc={"privileged": True}))
check(code == 403 and "Privileged containers are not allowed" in str(out), f"privileged refused ({code})")

code, out = req("ADMIN", "POST", "/api/v1/namespaces/shop/pods", pod("admin"))
check(code == 201 and out["metadata"].get("annotations", {}).get(SCC) == "anyuid"
      and "runAsUser" not in out["spec"]["containers"][0].get("securityContext", {}),
      f"the admin's pod: anyuid, unchanged ({code})")

setup(req("ADMIN", "POST", "/api/v1/namespaces/shop/serviceaccounts",
          {"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": "legacy"}}), "sa")
setup(req("ADMIN", "POST", "/apis/rbac.authorization.k8s.io/v1/namespaces/shop/rolebindings", {
    "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding", "metadata": {"name": "legacy-anyuid"},
    "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "system:openshift:scc:anyuid"},
    "subjects": [{"kind": "ServiceAccount", "name": "legacy", "namespace": "shop"}]}), "anyuid binding")
time.sleep(1)
code, out = req("ALICE", "POST", "/api/v1/namespaces/shop/pods", pod("legacy", csc={"runAsUser": 0}, sa="legacy"))
check(code == 201 and out["metadata"].get("annotations", {}).get(SCC) == "anyuid",
      f"SA granted anyuid: UID 0 admitted ({code} {out.get('message', '') if isinstance(out, dict) else ''})")

namespace("storm-tools")
code, out = req("ALICE", "POST", "/api/v1/namespaces/storm-tools/pods", pod("plain"))
check(code == 201 and SCC not in out["metadata"].get("annotations", {}), f"storm-* is not governed ({code})")
namespace("open", {"pod-security.kubernetes.io/enforce": "privileged"})
code, out = req("ALICE", "POST", "/api/v1/namespaces/open/pods", pod("root", csc={"runAsUser": 0}))
check(code == 201, f"enforce=privileged namespace is not governed ({code})")

# PodSecurity on updates (#361): the namespace's level now, on a significant update.
HP = "/api/v1/namespaces/open/pods/hostnet"
code, out = req("ADMIN", "POST", "/api/v1/namespaces/open/pods", pod("hostnet", {"hostNetwork": True}))
check(code == 201, f"a hostNetwork pod under enforce=privileged ({code})")
code, _ = req("ADMIN", "PATCH", "/api/v1/namespaces/open", {"metadata": {"labels": {"pod-security.kubernetes.io/enforce": "baseline"}}},
              "application/merge-patch+json")
check(code == 200, f"the namespace now enforces baseline ({code})")
code, out = req("ADMIN", "PATCH", HP, {"spec": {"containers": [{"name": "app", "image": "registry.example/app:2"}]}},
                "application/strategic-merge-patch+json")
img = req("ADMIN", "GET", HP)[1]["spec"]["containers"][0]["image"]
check(code == 403 and "hostNetwork is not allowed" in str(out) and img == "registry.example/app:1",
      f"a new image must meet baseline now: 403, unchanged ({code} {img})")
code, out = req("ADMIN", "PATCH", HP, {"spec": {"tolerations": [{"key": "k", "operator": "Exists"}]}},
                "application/strategic-merge-patch+json")
check(code == 200, f"a toleration, which PodSecurity does not consider, is still allowed ({code})")
sys.exit(failed)
PY
report
