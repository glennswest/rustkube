#!/usr/bin/env bash
#
# Custom-resource status isolation over HTTP (#128; the flowsdn dependency
# audit's C12): a real apiserver on fastetcd, four CRDs — namespaced and
# cluster-scoped, each with and without the `status` subresource.
#
# With the subresource:
#   - main POST stores no status, whatever the body says
#   - main PUT, merge PATCH, JSON PATCH and server-side apply change spec and
#     leave the stored status as it was
#   - PUT/PATCH /status change status and leave spec; a /status PUT with a
#     stale resourceVersion is a 409 (#78)
# Without it, status is an ordinary field: POST, PUT and PATCH store it.
#
# metadata.generation (#198): 1 on create whatever the body says; +1 when
# spec changes (with the subresource) or anything outside metadata does
# (without); unchanged by metadata-only writes and by /status.
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"]); TOKEN = os.environ["ADMIN"]
CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(method, path, body=None, ctype="application/json"):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    data = None if body is None else (body if isinstance(body, (str, bytes)) else json.dumps(body))
    conn.request(method, path, body=data,
                 headers={"Authorization": "Bearer " + TOKEN, "Content-Type": ctype})
    r = conn.getresponse(); raw = r.read(); conn.close()
    try:
        return r.status, json.loads(raw)
    except ValueError:
        return r.status, raw

def crd(plural, scope, status):
    version = {"name": "v1", "served": True, "storage": True,
               "schema": {"openAPIV3Schema": {"type": "object", "x-kubernetes-preserve-unknown-fields": True}}}
    if status:
        version["subresources"] = {"status": {}}
    kind = plural[:-1].capitalize()
    code, out = req("POST", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions", {
        "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
        "metadata": {"name": f"{plural}.c12.example.com"},
        "spec": {"group": "c12.example.com", "scope": scope, "versions": [version],
                 "names": {"plural": plural, "singular": plural[:-1], "kind": kind, "listKind": kind + "List"}}})
    if code >= 300:
        print(f"setup: CRD {plural}: {code} {out}"); sys.exit(100)
    return kind

cases = [("nswidgets", "Namespaced", True), ("clwidgets", "Cluster", True),
         ("nsplains", "Namespaced", False), ("clplains", "Cluster", False)]
kinds = {p: crd(p, s, st) for p, s, st in cases}
# Served once the registry has it.
for p, scope, _ in cases:
    base = "/apis/c12.example.com/v1" + ("/namespaces/default" if scope == "Namespaced" else "") + "/" + p
    for _ in range(60):
        if req("GET", base)[0] == 200: break
        time.sleep(0.5)

for plural, scope, has_status in cases:
    base = "/apis/c12.example.com/v1" + ("/namespaces/default" if scope == "Namespaced" else "") + "/" + plural
    tag = f"{plural} ({scope}, {'status subresource' if has_status else 'no subresource'})"
    obj = lambda name, spec, status: {"apiVersion": "c12.example.com/v1", "kind": kinds[plural],
        "metadata": {"name": name}, "spec": spec, "status": status}

    code, out = req("POST", base, obj("a", {"size": 1}, {"phase": "FromCreate"}))
    check(code == 201, f"{tag}: create {code}")
    gen = lambda: req("GET", base + "/a")[1]["metadata"].get("generation")
    check(out["metadata"].get("generation") == 1, f"{tag}: create sets generation 1 ({out['metadata'].get('generation')})")
    if has_status:
        check("status" not in out, f"{tag}: POST drops the body's status (got {out.get('status')})")
        # The controller reports, through /status.
        code, out = req("PUT", base + "/a/status", {**out, "status": {"phase": "Ready"}})
        check(code == 200 and out.get("status") == {"phase": "Ready"} and out["spec"] == {"size": 1},
              f"{tag}: PUT /status sets status, keeps spec ({code} {out.get('status')} {out.get('spec')})")
        check(gen() == 1, f"{tag}: /status leaves generation ({gen()})")
        code, out = req("PATCH", base + "/a", {"metadata": {"labels": {"l": "1"}, "generation": 42}},
                        "application/merge-patch+json")
        check(code == 200 and gen() == 1, f"{tag}: a metadata-only PATCH (and a sent generation) leaves it ({gen()})")
        cur = out
        code, out = req("PUT", base + "/a", {**cur, "spec": {"size": 2}, "status": {"phase": "Hacked"}})
        check(code == 200 and out["spec"] == {"size": 2} and out.get("status") == {"phase": "Ready"},
              f"{tag}: main PUT changes spec, keeps status ({out.get('spec')} {out.get('status')})")
        check(out["metadata"].get("generation") == 2, f"{tag}: a spec PUT bumps generation to 2 ({out['metadata'].get('generation')})")
        code, out = req("PATCH", base + "/a", {"spec": {"size": 3}, "status": {"phase": "Hacked"}},
                        "application/merge-patch+json")
        check(code == 200 and out["spec"] == {"size": 3} and out.get("status") == {"phase": "Ready"},
              f"{tag}: merge PATCH changes spec, keeps status ({out.get('spec')} {out.get('status')})")
        code, out = req("PATCH", base + "/a", [{"op": "replace", "path": "/spec/size", "value": 4},
                        {"op": "replace", "path": "/status", "value": {"phase": "Hacked"}}],
                        "application/json-patch+json")
        check(code == 200 and out["spec"] == {"size": 4} and out.get("status") == {"phase": "Ready"},
              f"{tag}: JSON PATCH changes spec, keeps status ({out.get('spec')} {out.get('status')})")
        apply = (f"apiVersion: c12.example.com/v1\nkind: {kinds[plural]}\nmetadata: {{name: a}}\n"
                 "spec: {size: 5}\nstatus: {phase: Hacked}\n")
        code, out = req("PATCH", base + "/a?fieldManager=c12&force=true", apply, "application/apply-patch+yaml")
        check(code == 200 and out["spec"] == {"size": 5} and out.get("status") == {"phase": "Ready"},
              f"{tag}: server-side apply changes spec, keeps status ({out.get('spec')} {out.get('status')})")
        apply_new = apply.replace("{name: a}", "{name: b}")
        code, out = req("PATCH", base + "/b?fieldManager=c12", apply_new, "application/apply-patch+yaml")
        check(code in (200, 201) and "status" not in out, f"{tag}: apply-create drops status ({code} {out.get('status')})")
        # /status: patch, then a stale PUT.
        code, out = req("PATCH", base + "/a/status", {"spec": {"size": 99}, "status": {"phase": "Degraded"}},
                        "application/merge-patch+json")
        check(code == 200 and out.get("status") == {"phase": "Degraded"} and out["spec"] == {"size": 5},
              f"{tag}: PATCH /status sets status, keeps spec ({out.get('status')} {out.get('spec')})")
        # PUT, merge, JSON patch and apply each changed spec once: 2..5.
        check(gen() == 5, f"{tag}: four spec writes, generation 5 ({gen()})")
        stale = {**cur, "status": {"phase": "Stale"}}
        code, out = req("PUT", base + "/a/status", stale)
        check(code == 409, f"{tag}: /status PUT with a stale resourceVersion is 409 (got {code})")
        code, out = req("GET", base + "/a")
        check(out.get("status") == {"phase": "Degraded"} and out["spec"] == {"size": 5},
              f"{tag}: stored after all that: spec 5, status Degraded ({out.get('spec')} {out.get('status')})")
    else:
        check(out.get("status") == {"phase": "FromCreate"}, f"{tag}: POST stores status ({out.get('status')})")
        code, out = req("PUT", base + "/a", {**out, "spec": {"size": 2}, "status": {"phase": "FromPut"}})
        check(code == 200 and out.get("status") == {"phase": "FromPut"} and out["spec"] == {"size": 2},
              f"{tag}: main PUT writes status ({out.get('status')})")
        check(out["metadata"].get("generation") == 2, f"{tag}: a PUT changing spec and status bumps to 2 ({out['metadata'].get('generation')})")
        code, out = req("PATCH", base + "/a", {"status": {"phase": "FromPatch"}}, "application/merge-patch+json")
        check(code == 200 and out.get("status") == {"phase": "FromPatch"}, f"{tag}: merge PATCH writes status ({out.get('status')})")
        check(out["metadata"].get("generation") == 3, f"{tag}: without the subresource a status change bumps it ({out['metadata'].get('generation')})")
        code, out = req("PATCH", base + "/a", {"metadata": {"annotations": {"a": "1"}}}, "application/merge-patch+json")
        check(code == 200 and out["metadata"].get("generation") == 3, f"{tag}: metadata-only PATCH leaves it ({out['metadata'].get('generation')})")
        code, out = req("PUT", base + "/a", {**out, "spec": {"size": 3}, "status": None})
        out2 = req("GET", base + "/a")[1]
        check(out2.get("status") in (None, {}), f"{tag}: main PUT without status clears it ({out2.get('status')})")

sys.exit(failed)
PY
report
