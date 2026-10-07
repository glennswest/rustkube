#!/usr/bin/env bash
#
# A WATCH ends at its timeoutSeconds (#165), against a real apiserver on
# fastetcd.
#
# - an idle ConfigMap watch with timeoutSeconds=3 ends cleanly (EOF, no
#   error) after 3 s and well before 10 s
# - an object created before the deadline arrives as ADDED first
# - a custom-resource watch and a watch with sendInitialEvents end the same way
# - without timeoutSeconds a watch is still open after 6 s
# - timeoutSeconds=abc is 400
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, socket, ssl, sys, threading, time, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
H = {"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": "application/json"}
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body), headers=H)
    r = c.getresponse(); raw = r.read(); c.close()
    return r.status, (json.loads(raw) if raw else None)
def watch(path, socket_timeout=12):
    """(seconds until EOF or None if the socket timed out, event lines)"""
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=socket_timeout)
    start = time.monotonic(); c.request("GET", path, headers=H); r = c.getresponse()
    events = []
    try:
        while True:
            line = r.fp.readline()
            if not line:
                return time.monotonic() - start, events, r.status
            events.append(json.loads(line))
    except (socket.timeout, TimeoutError):
        return None, events, r.status
    finally:
        c.close()

CM = "/api/v1/namespaces/default/configmaps"
took, events, status = watch(CM + "?watch=true&resourceVersion=1&timeoutSeconds=3&labelSelector=e2e%3Dnone")
check(status == 200 and took is not None and 2.9 <= took < 6, f"idle watch, timeoutSeconds=3: clean EOF ({took})")

_, cm = req("GET", CM)
rv = cm["metadata"]["resourceVersion"]
t = threading.Timer(1.0, lambda: req("POST", CM, {"apiVersion": "v1", "kind": "ConfigMap",
                                                  "metadata": {"name": "before-deadline", "labels": {"e2e": "wt"}}}))
t.start()
took, events, _ = watch(f"{CM}?watch=true&resourceVersion={rv}&timeoutSeconds=4&labelSelector=e2e%3Dwt")
check(took is not None and 3.9 <= took < 7 and [e["type"] for e in events] == ["ADDED"]
      and events[0]["object"]["metadata"]["name"] == "before-deadline",
      f"an event before the deadline is delivered, then EOF ({took}, {[e['type'] for e in events]})")

took, events, _ = watch(CM + "?watch=true&sendInitialEvents=true&allowWatchBookmarks=true&resourceVersionMatch=NotOlderThan&timeoutSeconds=3&labelSelector=e2e%3Dwt")
check(took is not None and 2.9 <= took < 6 and events and events[-1]["type"] == "BOOKMARK",
      f"WatchList with timeoutSeconds: initial events, bookmark, EOF ({took})")

crd = {"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
       "metadata": {"name": "widgets.wt.example.com"},
       "spec": {"group": "wt.example.com", "scope": "Namespaced",
                "names": {"plural": "widgets", "singular": "widget", "kind": "Widget"},
                "versions": [{"name": "v1", "served": True, "storage": True,
                              "schema": {"openAPIV3Schema": {"type": "object", "x-kubernetes-preserve-unknown-fields": True}}}]}}
req("POST", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions", crd)
for _ in range(50):
    if req("GET", "/apis/wt.example.com/v1/namespaces/default/widgets")[0] == 200: break
    time.sleep(0.2)
took, _, status = watch("/apis/wt.example.com/v1/namespaces/default/widgets?watch=true&timeoutSeconds=3")
check(status == 200 and took is not None and 2.9 <= took < 6, f"custom-resource watch ends at timeoutSeconds ({status} {took})")

took, _, _ = watch(CM + "?watch=true&resourceVersion=1&labelSelector=e2e%3Dnone", socket_timeout=6)
check(took is None, f"without timeoutSeconds the watch stays open ({took})")

code, out = req("GET", CM + "?watch=true&timeoutSeconds=abc")
check(code == 400, f"timeoutSeconds=abc: 400 ({code})")
sys.exit(failed)
PY
report
