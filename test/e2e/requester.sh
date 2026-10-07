#!/usr/bin/env bash
#
# The apiserver stamps who created a storage.storm.io object (#210), so
# stormdrive's controller can SubjectAccessReview the requester of a
# DriveOperation (stormdrive#45) without trusting the object. A real
# apiserver on fastetcd; alice and bob are ordinary users granted the CRDs.
#
# - create (POST, server-side apply): storage.storm.io/requester and
#   requester-groups name the caller, whatever the body forged
# - PUT, merge PATCH, JSON PATCH (remove), server-side apply, PUT /status
#   by someone else, forging or dropping them: the stamp is unchanged
# - namespaced and cluster-scoped kinds alike; another group's object keeps
#   the annotation its client wrote
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
ALICE=$(token alice '["storage-admins"]')
BOB=$(token bob '["storage-admins"]')
export API ADMIN ALICE BOB
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"])
CTX = ssl._create_unverified_context()
R, G = "storage.storm.io/requester", "storage.storm.io/requester-groups"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(who, method, path, body=None, ctype="application/json"):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    data = None if body is None else (body if isinstance(body, str) else json.dumps(body))
    conn.request(method, path, body=data,
                 headers={"Authorization": "Bearer " + os.environ[who], "Content-Type": ctype})
    r = conn.getresponse(); raw = r.read(); conn.close()
    try: return r.status, json.loads(raw)
    except ValueError: return r.status, raw

def setup(code_out, what):
    code, out = code_out
    if code >= 300 and code != 409:
        print(f"setup {what}: {code} {out}"); sys.exit(100)

def crd(group, plural, kind, scope):
    setup(req("ADMIN", "POST", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions", {
        "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
        "metadata": {"name": f"{plural}.{group}"},
        "spec": {"group": group, "scope": scope,
                 "names": {"plural": plural, "singular": plural[:-1], "kind": kind, "listKind": kind + "List"},
                 "versions": [{"name": "v1", "served": True, "storage": True, "subresources": {"status": {}},
                               "schema": {"openAPIV3Schema": {"type": "object",
                                          "x-kubernetes-preserve-unknown-fields": True}}}]}}), plural)
crd("storage.storm.io", "driveoperations", "DriveOperation", "Cluster")
crd("storage.storm.io", "raidsets", "RaidSet", "Namespaced")
crd("other.example.com", "things", "Thing", "Namespaced")
setup(req("ADMIN", "POST", "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings", {
    "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding",
    "metadata": {"name": "storage-admins"},
    "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "cluster-admin"},
    "subjects": [{"kind": "Group", "name": "storage-admins", "apiGroup": "rbac.authorization.k8s.io"}]}), "binding")

DO = "/apis/storage.storm.io/v1/driveoperations"
RS = "/apis/storage.storm.io/v1/namespaces/default/raidsets"
TH = "/apis/other.example.com/v1/namespaces/default/things"
for p in (DO, RS, TH):
    for _ in range(60):
        if req("ADMIN", "GET", p)[0] == 200: break
        time.sleep(0.5)

forged = {R: "system:admin", G: "system:masters", "note": "mine"}
def stamp(path, name):
    a = req("ADMIN", "GET", f"{path}/{name}")[1].get("metadata", {}).get("annotations", {})
    return a.get(R), a.get(G), a.get("note")

def is_alice(s):
    return s[0] == "alice" and "storage-admins" in (s[1] or "").split(",") and "system:masters" not in (s[1] or "")

for path, kind, tag in ((DO, "DriveOperation", "cluster"), (RS, "RaidSet", "namespaced")):
    obj = lambda name, ann, spec: {"apiVersion": "storage.storm.io/v1", "kind": kind,
                                   "metadata": {"name": name, "annotations": ann}, "spec": spec}
    code, out = req("ALICE", "POST", path, obj("op1", forged, {"drive": "sda", "action": "format"}))
    check(code == 201, f"{tag}: alice creates op1 ({code})")
    s = stamp(path, "op1")
    check(is_alice(s), f"{tag}: POST stamps alice over the forged values ({s[0]!r}, {s[1]!r})")
    check(s[2] == "mine", f"{tag}: the body's other annotations are kept ({s[2]!r})")

    cur = req("BOB", "GET", f"{path}/op1")[1]
    cur["metadata"]["annotations"] = {R: "bob", G: "system:masters"}
    cur["spec"]["drive"] = "sdb"
    code, out = req("BOB", "PUT", f"{path}/op1", cur)
    s = stamp(path, "op1")
    check(code == 200 and is_alice(s) and out["spec"]["drive"] == "sdb",
          f"{tag}: bob's PUT changes spec, not the stamp ({code} {s[0]!r})")
    code, _ = req("BOB", "PATCH", f"{path}/op1", {"metadata": {"annotations": {R: "bob", G: None}}},
                  "application/merge-patch+json")
    s = stamp(path, "op1")
    check(code == 200 and is_alice(s), f"{tag}: merge PATCH forging/removing: stamp unchanged ({code} {s[0]!r} {s[1]!r})")
    code, _ = req("BOB", "PATCH", f"{path}/op1",
                  [{"op": "remove", "path": "/metadata/annotations/storage.storm.io~1requester"}],
                  "application/json-patch+json")
    s = stamp(path, "op1")
    check(code == 200 and is_alice(s), f"{tag}: JSON PATCH remove: stamp unchanged ({code} {s[0]!r})")
    apply = (f"apiVersion: storage.storm.io/v1\nkind: {kind}\nmetadata:\n  name: op1\n"
             f"  annotations: {{{R}: bob, {G}: 'system:masters'}}\nspec: {{drive: sdc}}\n")
    code, _ = req("BOB", "PATCH", f"{path}/op1?fieldManager=bob&force=true", apply, "application/apply-patch+yaml")
    s = stamp(path, "op1")
    check(code == 200 and is_alice(s), f"{tag}: server-side apply forging: stamp unchanged ({code} {s[0]!r})")
    cur = req("BOB", "GET", f"{path}/op1")[1]
    cur["metadata"]["annotations"] = {R: "bob"}
    cur["status"] = {"phase": "Running"}
    code, out = req("BOB", "PUT", f"{path}/op1/status", cur)
    s = stamp(path, "op1")
    check(code == 200 and is_alice(s), f"{tag}: PUT /status forging: stamp unchanged ({code} {s[0]!r})")
    code, out = req("BOB", "PATCH", f"{path}/op2?fieldManager=bob", apply.replace("name: op1", "name: op2"),
                    "application/apply-patch+yaml")
    s = stamp(path, "op2")
    check(code in (200, 201) and s[0] == "bob" and "system:masters" not in (s[1] or ""),
          f"{tag}: server-side apply creating op2 stamps bob ({code} {s[0]!r} {s[1]!r})")

# The admin's own create is stamped too (system:masters is who it is).
code, _ = req("ADMIN", "POST", DO, {"apiVersion": "storage.storm.io/v1", "kind": "DriveOperation",
                                    "metadata": {"name": "op-admin"}, "spec": {}})
s = stamp(DO, "op-admin")
check(code == 201 and s[0] == "admin" and "system:masters" in (s[1] or ""), f"admin's create stamped ({s[0]!r} {s[1]!r})")

# Another group: the client's annotation is its own business.
code, _ = req("ALICE", "POST", TH, {"apiVersion": "other.example.com/v1", "kind": "Thing",
                                    "metadata": {"name": "t1", "annotations": forged}, "spec": {}})
s = stamp(TH, "t1")
check(code == 201 and s[0] == "system:admin", f"other group: annotations as the client wrote them ({s[0]!r})")
sys.exit(failed)
PY
report
