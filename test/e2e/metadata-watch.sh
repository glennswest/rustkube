#!/usr/bin/env bash
#
# Metadata-only CRD watches, as client-go's metadata informer decodes them
# (#180: the cilium agent on server1 logged "unable to decode an event from
# the watch stream" on its `as=PartialObjectMetadata` CRD watch). A real
# apiserver on fastetcd, no controllers. Three watches on
# customresourcedefinitions with cilium's Accept header:
#
#   plain      allowWatchBookmarks=true, from the LIST's revision
#   watchlist  sendInitialEvents=true (initial ADDED + initial-events-end)
#   old        from resourceVersion 1: below the watch cache's window, so the
#              store's watch, with key-only DELETED tombstones
#
# While they run: CRDs created, one updated, one deleted, then 50 s quiet for
# the heartbeat BOOKMARK. Every frame must be one line of JSON that decodes as
# meta.k8s.io/v1 PartialObjectMetadata (or an ERROR Status), with every
# ObjectMeta field of Go's type; each watch must see the create and the
# delete, the plain one a BOOKMARK, the watchlist one its end marker.
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import datetime, http.client, json, os, ssl, sys, threading, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"])
CTX = ssl._create_unverified_context()
ACCEPT = "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1"
CRDS = "/apis/apiextensions.k8s.io/v1/customresourcedefinitions"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(method, path, body=None, ctype="application/json", accept="application/json"):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype, "Accept": accept})
    r = conn.getresponse(); raw = r.read(); conn.close()
    return r.status, json.loads(raw) if raw else None

def rfc3339(v):
    if v is None: return True
    try: datetime.datetime.fromisoformat(v.replace("Z", "+00:00")); return True
    except (ValueError, AttributeError): return False

def go_meta(m):
    """What encoding/json into metav1.ObjectMeta refuses."""
    if not isinstance(m, dict): return "metadata is not an object"
    for k, v in m.items():
        if k in ("name", "generateName", "namespace", "selfLink", "uid", "resourceVersion"):
            ok = v is None or isinstance(v, str)
        elif k in ("generation", "deletionGracePeriodSeconds"):
            ok = v is None or (isinstance(v, int) and not isinstance(v, bool))
        elif k in ("creationTimestamp", "deletionTimestamp"):
            ok = rfc3339(v)
        elif k in ("labels", "annotations"):
            ok = v is None or (isinstance(v, dict) and all(isinstance(x, str) for x in v.values()))
        elif k == "finalizers":
            ok = v is None or (isinstance(v, list) and all(isinstance(x, str) for x in v))
        elif k == "ownerReferences":
            ok = v is None or all(isinstance(r, dict) and all(
                isinstance(x, bool) or x is None if f in ("controller", "blockOwnerDeletion") else isinstance(x, str)
                for f, x in r.items()) for r in v)
        elif k == "managedFields":
            ok = v is None or all(isinstance(e, dict) and all(
                rfc3339(x) if f == "time" else (isinstance(x, dict) or x is None) if f == "fieldsV1" else (x is None or isinstance(x, str))
                for f, x in e.items()) for e in v)
        else:
            ok = True
        if not ok: return f"metadata.{k} = {v!r}"
    return None

def frame_error(line):
    try: ev = json.loads(line)
    except ValueError as e: return f"not one JSON object: {e}"
    t, o = ev.get("type"), ev.get("object")
    if t == "ERROR":
        return None if isinstance(o, dict) and o.get("kind") == "Status" else "ERROR without a Status"
    if t not in ("ADDED", "MODIFIED", "DELETED", "BOOKMARK"): return f"type {t!r}"
    if not isinstance(o, dict) or o.get("apiVersion") != "meta.k8s.io/v1" or o.get("kind") != "PartialObjectMetadata":
        return f"{t} object is {o.get('apiVersion') if isinstance(o, dict) else o!r} {o.get('kind') if isinstance(o, dict) else ''}"
    extra = set(o) - {"apiVersion", "kind", "metadata"}
    if extra: return f"{t} object carries {sorted(extra)}"
    return go_meta(o.get("metadata"))

class Watch(threading.Thread):
    def __init__(self, name, query):
        super().__init__(daemon=True)
        self.name, self.query, self.frames, self.errors = name, query, [], []
    def run(self):
        conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=120)
        conn.request("GET", f"{CRDS}?watch=true&{self.query}",
                     headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Accept": ACCEPT})
        r = conn.getresponse()
        if r.status != 200:
            self.errors.append(f"HTTP {r.status}: {r.read()[:300]!r}"); return
        buf = b""
        while True:
            try: chunk = r.read1(65536)
            except Exception as e: self.errors.append(f"stream ended: {e!r}"); return
            if not chunk: return
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                text = line.decode()
                err = frame_error(text)
                if err: self.errors.append(f"{err}: {text[:300]}")
                else: self.frames.append(json.loads(text))

def crd(plural):
    return {"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
            "metadata": {"name": f"{plural}.mw.example.com", "labels": {"io.cilium/app": "x"},
                         "annotations": {"note": "metadata-watch"}},
            "spec": {"group": "mw.example.com", "scope": "Namespaced",
                     "names": {"plural": plural, "singular": plural[:-1], "kind": plural.capitalize()[:-1], "listKind": plural.capitalize()[:-1] + "List"},
                     "versions": [{"name": "v1", "served": True, "storage": True,
                                   "schema": {"openAPIV3Schema": {"type": "object", "x-kubernetes-preserve-unknown-fields": True}}}]}}

req("POST", CRDS, crd("seeds"))
_, lst = req("GET", CRDS, accept=ACCEPT)
check(lst["kind"] == "PartialObjectMetadataList" and all(frame_error(json.dumps({"type": "ADDED", "object": i})) is None for i in lst["items"]),
      f"metadata-only LIST: {lst['kind']}, {len(lst['items'])} items decode")
rv = lst["metadata"]["resourceVersion"]
watches = [Watch("plain", f"allowWatchBookmarks=true&resourceVersion={rv}"),
           Watch("watchlist", "allowWatchBookmarks=true&sendInitialEvents=true&resourceVersionMatch=NotOlderThan&resourceVersion="),
           Watch("old", "allowWatchBookmarks=true&resourceVersion=1")]
for w in watches: w.start()
time.sleep(2)

req("POST", CRDS, crd("widgets"))
req("POST", CRDS, crd("gadgets"))
req("PATCH", f"{CRDS}/widgets.mw.example.com", {"metadata": {"labels": {"changed": "yes"}}}, "application/merge-patch+json")
req("DELETE", f"{CRDS}/gadgets.mw.example.com")
req("DELETE", f"{CRDS}/seeds.mw.example.com")
print("waiting 50 s for the heartbeat BOOKMARK…", flush=True)
time.sleep(50)

for w in watches:
    check(not w.errors, f"{w.name}: every frame decodes as PartialObjectMetadata ({len(w.frames)} frames){'; ' + '; '.join(w.errors[:3]) if w.errors else ''}")
    names = lambda t: {f["object"]["metadata"].get("name") for f in w.frames if f["type"] == t}
    check("widgets.mw.example.com" in names("ADDED"), f"{w.name}: sees the create")
    check({"gadgets.mw.example.com", "seeds.mw.example.com"} <= names("DELETED"), f"{w.name}: sees both deletes ({sorted(names('DELETED'))})")
    check(any(f["type"] == "BOOKMARK" for f in w.frames), f"{w.name}: a BOOKMARK")
wl = watches[1]
check(any(f["type"] == "BOOKMARK" and f["object"]["metadata"].get("annotations", {}).get("k8s.io/initial-events-end") == "true" for f in wl.frames),
      "watchlist: the initial-events-end BOOKMARK")
sys.exit(failed)
PY
report
