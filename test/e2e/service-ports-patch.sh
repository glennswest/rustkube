#!/usr/bin/env bash
#
# Strategic merge patches of a Service's ports (#150), against a real
# apiserver on fastetcd, with the bodies `kubectl patch` sends. A ServicePort
# has no `containerPort`, the key every `ports` list was merged on, so a patch
# to one port appended a second entry instead of changing it.
#
# - one port of a two-port Service patched: still two ports, that one changed
# - 53/UDP and 53/TCP stay two entries; a patch to the UDP one leaves TCP
# - `$patch: delete` removes by port + protocol (protocol defaults to TCP)
# - `$setElementOrder/ports` reorders by the same key
# - a container's ports (a Deployment's template) still merge by containerPort
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
SMP = "application/strategic-merge-patch+json"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, ctype="application/json"):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype})
    r = c.getresponse(); raw = r.read(); c.close()
    try: return r.status, json.loads(raw)
    except ValueError: return r.status, raw
S = "/api/v1/namespaces/default/services"
def svc(name, ports):
    return req("POST", S, {"apiVersion": "v1", "kind": "Service", "metadata": {"name": name},
                           "spec": {"selector": {"app": name}, "ports": ports}})
def ports(o):
    return [(p["port"], p.get("protocol"), p.get("name"), p.get("targetPort")) for p in o["spec"]["ports"]]

# --- one port of two -----------------------------------------------------------
code, _ = svc("web", [{"name": "http", "port": 80, "targetPort": 8080},
                      {"name": "https", "port": 443, "targetPort": 8443}])
check(code == 201, f"two-port Service created ({code})")
code, o = req("PATCH", S + "/web", {"spec": {"ports": [{"port": 443, "targetPort": 9443}]}}, SMP)
check(code == 200 and ports(o) == [(80, "TCP", "http", 8080), (443, "TCP", "https", 9443)],
      f"a patch to port 443 changes it in place: two ports ({code} {ports(o) if code == 200 else o})")
code, o = req("PATCH", S + "/web", {"spec": {"ports": [{"port": 8000, "name": "alt", "targetPort": 8000}]}}, SMP)
check(code == 200 and [p[0] for p in ports(o)] == [80, 443, 8000],
      f"a patch naming a new port adds it ({code} {ports(o) if code == 200 else o})")

# --- one number, two protocols ---------------------------------------------------
code, _ = svc("dns", [{"name": "dns-tcp", "port": 53, "protocol": "TCP"},
                      {"name": "dns", "port": 53, "protocol": "UDP"}])
check(code == 201, f"53/TCP + 53/UDP Service created ({code})")
code, o = req("PATCH", S + "/dns", {"spec": {"ports": [{"port": 53, "protocol": "UDP", "targetPort": 5353}]}}, SMP)
check(code == 200 and ports(o) == [(53, "TCP", "dns-tcp", 53), (53, "UDP", "dns", 5353)],
      f"a patch to 53/UDP leaves 53/TCP ({code} {ports(o) if code == 200 else o})")

# --- delete and order --------------------------------------------------------------
code, o = req("PATCH", S + "/web", {"spec": {"ports": [{"port": 80, "$patch": "delete"}]}}, SMP)
check(code == 200 and [p[0] for p in ports(o)] == [443, 8000],
      f"$patch: delete removes port 80 (TCP by default) ({code} {ports(o) if code == 200 else o})")
code, o = req("PATCH", S + "/dns", {"spec": {"ports": [{"port": 53, "protocol": "TCP", "$patch": "delete"}]}}, SMP)
check(code == 200 and ports(o) == [(53, "UDP", "dns", 5353)],
      f"$patch: delete of 53/TCP keeps 53/UDP ({code} {ports(o) if code == 200 else o})")
code, o = req("PATCH", S + "/web", {"spec": {"$setElementOrder/ports": [{"port": 8000}, {"port": 443}],
                                             "ports": [{"port": 443, "name": "https"}]}}, SMP)
check(code == 200 and [p[0] for p in ports(o)] == [8000, 443],
      f"$setElementOrder/ports reorders by port ({code} {ports(o) if code == 200 else o})")

# --- a container's ports keep containerPort ----------------------------------------
D = "/apis/apps/v1/namespaces/default/deployments"
code, _ = req("POST", D, {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "app"},
    "spec": {"replicas": 0, "selector": {"matchLabels": {"app": "app"}},
             "template": {"metadata": {"labels": {"app": "app"}},
                          "spec": {"containers": [{"name": "c", "image": "unused",
                                   "ports": [{"containerPort": 80, "name": "http"},
                                             {"containerPort": 9090, "name": "metrics"}]}]}}}})
check(code == 201, f"Deployment with two container ports created ({code})")
code, o = req("PATCH", D + "/app", {"spec": {"template": {"spec": {"containers": [
    {"name": "c", "ports": [{"containerPort": 80, "name": "web"}]}]}}}}, SMP)
cp = [(p["containerPort"], p.get("name")) for p in o["spec"]["template"]["spec"]["containers"][0]["ports"]] if code == 200 else o
check(code == 200 and cp == [(80, "web"), (9090, "metrics")],
      f"container ports still merge by containerPort ({code} {cp})")
sys.exit(failed)
PY
report
