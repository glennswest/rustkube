#!/usr/bin/env bash
#
# LIST and GET from the watch cache (#171; #5's acceptance): a real apiserver
# on fastetcd, no controllers.
#
# - A/B: a LIST and a GET with resourceVersion=0 (cache) answer what the same
#   read without one (datastore) answers — names, items' resourceVersions
# - resourceVersion=N after a write sees that write (LIST and GET)
# - a paged LIST from the cache (limit, continue) returns every object once,
#   every page at the first page's revision
# - resourceVersionMatch=Exact reads the store at that revision (the old data)
# - a non-numeric resourceVersion, or Exact without one, is a 400
# - a relist storm: 50 LISTs and 50 GETs with resourceVersion=0 add no
#   datastore Range (etcd_request_duration_seconds_count), and are counted in
#   apiserver_watch_cache_reads_total; 50 without one each add one Range
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, re, ssl, sys, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"])
CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(method, path, body=None, ctype="application/json"):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype})
    r = conn.getresponse(); raw = r.read(); conn.close()
    try: return r.status, json.loads(raw)
    except ValueError: return r.status, raw.decode(errors="replace")

def metric(name, **labels):
    """Sum of a metric's samples whose labels include `labels`."""
    _, text = req("GET", "/metrics")
    total = 0.0
    for line in text.splitlines():
        if not line.startswith(name):
            continue
        m = re.match(r'^(\w+)(\{[^}]*\})?\s+([0-9.eE+-]+)$', line)
        if not m or m.group(1) != name:
            continue
        got = dict(re.findall(r'(\w+)="([^"]*)"', m.group(2) or ""))
        if all(got.get(k) == v for k, v in labels.items()):
            total += float(m.group(3))
    return total

for ns in ("cache-a", "cache-b"):
    req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": ns}})
for i in range(30):
    req("POST", "/api/v1/namespaces/cache-a/configmaps",
        {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": f"cm{i:02}"}, "data": {"v": "1"}})
for i in range(5):
    req("POST", "/api/v1/namespaces/cache-b/configmaps",
        {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": f"other{i}"}, "data": {}})

A = "/api/v1/namespaces/cache-a/configmaps"
def shape(lst):
    return sorted((i["metadata"]["name"], i["metadata"]["resourceVersion"]) for i in lst["items"])

# The cache's first answer seeds it; let it catch up with the creates.
time.sleep(1)
_, store = req("GET", A)
_, cached = req("GET", A + "?resourceVersion=0")
check(shape(store) == shape(cached) and len(store["items"]) == 30,
      f"A/B LIST: cache == store ({len(cached['items'])} vs {len(store['items'])} items)")
check(all(i["metadata"]["namespace"] == "cache-a" for i in cached["items"]), "cache LIST holds only its namespace")
_, s1 = req("GET", A + "/cm07")
_, c1 = req("GET", A + "/cm07?resourceVersion=0")
check(s1 == c1, "A/B GET: cache == store")
code, _ = req("GET", A + "/nope?resourceVersion=0")
check(code == 404, f"GET of a missing object from the cache is 404 ({code})")

# A write, then reads at its revision.
before_rv = store["metadata"]["resourceVersion"]
_, upd = req("PATCH", A + "/cm07", {"data": {"v": "2"}}, "application/merge-patch+json")
rv = upd["metadata"]["resourceVersion"]
_, g = req("GET", A + f"/cm07?resourceVersion={rv}")
check(g["data"]["v"] == "2" and g["metadata"]["resourceVersion"] == rv, f"GET resourceVersion=N sees the write ({g['data']})")
_, l = req("GET", A + f"?resourceVersion={rv}")
v = [i["data"]["v"] for i in l["items"] if i["metadata"]["name"] == "cm07"]
check(v == ["2"] and int(l["metadata"]["resourceVersion"]) >= int(rv), f"LIST resourceVersion=N sees the write ({v}, list rv {l['metadata']['resourceVersion']})")

# Exact: the store at the old revision.
code, ex = req("GET", A + f"?resourceVersion={before_rv}&resourceVersionMatch=Exact")
v = [i["data"]["v"] for i in ex.get("items", []) if i["metadata"]["name"] == "cm07"] if code == 200 else code
check(code == 200 and v == ["1"] and ex["metadata"]["resourceVersion"] == before_rv,
      f"LIST Exact reads the old revision ({v})")

# Paging from the cache.
names, revs, cont, pages = [], set(), "", 0
while True:
    q = "?resourceVersion=0&limit=7" + (f"&continue={urllib.parse.quote(cont)}" if cont else "")
    code, page = req("GET", A + q)
    if code != 200:
        break
    pages += 1
    names += [i["metadata"]["name"] for i in page["items"]]
    revs.add(page["metadata"]["resourceVersion"])
    cont = page["metadata"].get("continue", "")
    if not cont:
        break
check(sorted(names) == [f"cm{i:02}" for i in range(30)] and pages == 5 and len(revs) == 1,
      f"paged from the cache: 30 objects once in {pages} pages at one revision {revs}")

for q, what in (("?resourceVersion=abc", "non-numeric resourceVersion"),
                ("?resourceVersionMatch=Exact", "a match without a resourceVersion"),
                ("?resourceVersion=0&resourceVersionMatch=Exact", "Exact with 0")):
    code, _ = req("GET", A + q)
    check(code == 400, f"{what} is 400 ({code})")

# The relist storm.
lists0 = metric("etcd_request_duration_seconds_count", operation="list", type="configmaps")
gets0 = metric("etcd_request_duration_seconds_count", operation="get", type="configmaps")
cache0 = metric("apiserver_watch_cache_reads_total", type="configmaps")
for _ in range(50):
    req("GET", A + "?resourceVersion=0")
    req("GET", A + "/cm01?resourceVersion=0")
lists1 = metric("etcd_request_duration_seconds_count", operation="list", type="configmaps")
gets1 = metric("etcd_request_duration_seconds_count", operation="get", type="configmaps")
cache1 = metric("apiserver_watch_cache_reads_total", type="configmaps")
check(lists1 - lists0 == 0 and gets1 - gets0 == 0,
      f"50 LISTs + 50 GETs at resourceVersion=0: {lists1 - lists0:.0f} store LISTs, {gets1 - gets0:.0f} store GETs")
check(cache1 - cache0 >= 100, f"…all from the cache ({cache1 - cache0:.0f} cache reads)")
for _ in range(50):
    req("GET", A)
lists2 = metric("etcd_request_duration_seconds_count", operation="list", type="configmaps")
check(lists2 - lists1 >= 50, f"control: 50 consistent LISTs are {lists2 - lists1:.0f} store LISTs")
sys.exit(failed)
PY
report
