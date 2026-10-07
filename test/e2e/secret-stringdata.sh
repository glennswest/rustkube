#!/usr/bin/env bash
#
# A Secret's stringData is folded into data on every write (#101): a real
# apiserver on fastetcd. Create with data and stringData sharing a key, merge
# PATCH and PUT carrying stringData: each time the stored Secret has the value
# base64 in data (stringData winning a shared key) and no stringData, and a
# GET and a LIST return the same. Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import base64, http.client, json, os, ssl, sys, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, ctype="application/json"):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype})
    r = conn.getresponse(); raw = r.read(); conn.close()
    return r.status, json.loads(raw) if raw else None
b64 = lambda s: base64.b64encode(s.encode()).decode()
S = "/api/v1/namespaces/default/secrets"
code, out = req("POST", S, {"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "s1"},
    "data": {"keep": b64("k"), "both": b64("old")}, "stringData": {"both": "new", "userdata": "#cloud-config\n"}})
check(code == 201 and "stringData" not in out and out["data"] == {"keep": b64("k"), "both": b64("new"), "userdata": b64("#cloud-config\n")},
      f"create: folded, stringData wins a shared key, not returned ({code} {out.get('data') if isinstance(out, dict) else out})")
_, got = req("GET", f"{S}/s1")
check("stringData" not in got and got["data"]["userdata"] == b64("#cloud-config\n"), "GET returns data only")
code, out = req("PATCH", f"{S}/s1", {"stringData": {"patched": "p"}}, "application/merge-patch+json")
check(code == 200 and "stringData" not in out and out["data"].get("patched") == b64("p") and out["data"]["keep"] == b64("k"),
      f"merge PATCH of stringData is folded ({code})")
got["stringData"] = {"put": "q"}; del got["metadata"]["resourceVersion"]
code, out = req("PUT", f"{S}/s1", got)
check(code == 200 and "stringData" not in out and out["data"].get("put") == b64("q"), f"PUT with stringData is folded ({code})")
_, lst = req("GET", S)
s1 = [s for s in lst["items"] if s["metadata"]["name"] == "s1"][0]
check("stringData" not in s1 and s1["data"].get("put") == b64("q"), "LIST returns the folded Secret")
sys.exit(failed)
PY
report
