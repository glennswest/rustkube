#!/usr/bin/env bash
#
# The apiserver compacts the datastore, and paging and watches answer for it
# as upstream's do (#139, #127): a real apiserver on fastetcd, compacting
# every 3 s.
#
# - a LIST continue token works at first, then — once its snapshot is
#   compacted — is a 410 `Expired` Status carrying an "inconsistent" continue
#   token in metadata.continue (conformance: "should support continue listing
#   from the last key if the original version has been compacted away")
# - that token lists the rest at a newer revision, every page at the same
#   one, and sees an object created after the first page
# - a LIST at the old revision (resourceVersionMatch=Exact) is 410 `Expired`
# - a WATCH from below the compaction gets an ERROR event, 410 `Expired`,
#   which is what makes client-go relist (#127)
# - the compacted revision is exported as apiserver_storage_compacted_revision
#
# Exit status is the number of failed checks.
RK_APISERVER_ARGS="${RK_APISERVER_ARGS:-} --etcd-compaction-interval=3s"
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
def conn():
    return http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
def req(method, path, body=None):
    c = conn()
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": "application/json"})
    r = c.getresponse(); raw = r.read(); c.close()
    try: return r.status, json.loads(raw) if raw else None
    except ValueError: return r.status, raw.decode(errors="replace")
q = urllib.parse.quote
CM = "/api/v1/namespaces/default/configmaps"
def cm(name):
    code, out = req("POST", CM, {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "labels": {"rig": "compaction"}}})
    if code != 201: print(f"setup {name}: {code} {out}"); sys.exit(100)
for i in range(6): cm(f"cm-{i}")

SEL = "labelSelector=rig%3Dcompaction"
code, first = req("GET", f"{CM}?{SEL}&limit=2")
token, first_rv = first["metadata"].get("continue"), first["metadata"]["resourceVersion"]
check(code == 200 and len(first["items"]) == 2 and token, f"first page: 2 items and a continue token ({code})")
code, _ = req("GET", f"{CM}?{SEL}&limit=2&continue={q(token)}")
check(code == 200, f"the token works before a compaction ({code})")
cm("cm-9")  # created after the first page: only an inconsistent list sees it

# Within two intervals (+ slack) the token's snapshot is compacted.
deadline, status = time.time() + 30, None
while time.time() < deadline:
    code, out = req("GET", f"{CM}?{SEL}&limit=2&continue={q(token)}")
    if code != 200: status = (code, out); break
    time.sleep(1)
check(status is not None, f"the token expires within 30 s of 3 s compactions ({status and status[0]})")
code, out = status or (0, {})
inconsistent = (out.get("metadata") or {}).get("continue") if isinstance(out, dict) else None
check(code == 410 and isinstance(out, dict) and out.get("reason") == "Expired",
      f"410 Expired, as client-go's IsResourceExpired needs ({code} {out.get('reason') if isinstance(out, dict) else out})")
check(bool(inconsistent) and "inconsistent list" in out.get("message", ""),
      f"the Status carries an inconsistent continue token ({inconsistent!r})")

if inconsistent:
    names, rvs, cont = [], set(), inconsistent
    while cont:
        code, page = req("GET", f"{CM}?{SEL}&limit=2&continue={q(cont)}")
        if code != 200: check(False, f"paging with the inconsistent token: {code} {page}"); break
        names += [i["metadata"]["name"] for i in page["items"]]; rvs.add(page["metadata"]["resourceVersion"])
        cont = page["metadata"].get("continue")
    check(names == ["cm-2", "cm-3", "cm-4", "cm-5", "cm-9"],
          f"the rest is listed from the same key, including the newer cm-9 ({names})")
    check(len(rvs) == 1 and first_rv not in rvs, f"at one newer revision ({rvs} vs first {first_rv})")

code, out = req("GET", f"{CM}?{SEL}&resourceVersion={first_rv}&resourceVersionMatch=Exact")
check(code == 410 and isinstance(out, dict) and out.get("reason") == "Expired",
      f"LIST at the compacted revision (Exact): 410 Expired ({code} {out.get('reason') if isinstance(out, dict) else out})")

# A WATCH below the compaction, on a resource nothing else watches (so no
# watch cache holds it): one ERROR event, 410 Expired.
c = conn()
c.request("GET", f"/api/v1/namespaces/default/podtemplates?watch=1&resourceVersion={first_rv}",
          headers={"Authorization": "Bearer " + os.environ["ADMIN"]})
r = c.getresponse()
line = r.fp.readline() if r.status == 200 else r.read()
c.close()
try: ev = json.loads(line)
except ValueError: ev = {}
obj = ev.get("object") or {}
check(ev.get("type") == "ERROR" and obj.get("code") == 410 and obj.get("reason") == "Expired",
      f"WATCH from rv {first_rv}: ERROR 410 Expired ({r.status} {line[:200]!r})")

code, metrics = req("GET", "/metrics")
rev = [l for l in (metrics if isinstance(metrics, str) else "").splitlines() if l.startswith("apiserver_storage_compacted_revision")]
check(rev and float(rev[0].split()[-1]) > int(first_rv), f"apiserver_storage_compacted_revision past the first page ({rev})")
sys.exit(failed)
PY
report
