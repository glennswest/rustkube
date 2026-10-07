#!/usr/bin/env bash
#
# NodePort allocation and type changes (#132), against a real apiserver on
# fastetcd — the conformance specs' flows and the allocator's rules.
#
# - NodePort / LoadBalancer Services get a node port per port in 30000-32767
# - a named port is kept; the same one again is 422 "already allocated";
#   outside the range is 422; a ClusterIP Service naming one is 422
# - one port number may serve TCP and UDP of one Service
# - a PUT that leaves nodePort and clusterIP empty keeps them; a changed
#   clusterIP is 422
# - NodePort → ClusterIP drops the node ports and frees them; NodePort →
#   ExternalName drops the ClusterIP too; ExternalName → ClusterIP and →
#   NodePort allocate a ClusterIP (and node ports)
# - a strategic-merge PATCH changing the type does the same
# - delete frees the node ports
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
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
def svc(name, typ, ports, **spec):
    body = {"apiVersion": "v1", "kind": "Service", "metadata": {"name": name},
            "spec": dict(type=typ, ports=ports, **spec)}
    return req("POST", S, body)
np = lambda o: [p.get("nodePort", 0) for p in o["spec"]["ports"]]
inrange = lambda n: 30000 <= n <= 32767

code, a = svc("np-a", "NodePort", [{"port": 80}])
check(code == 201 and inrange(np(a)[0]), f"NodePort: a node port in range ({code} {np(a) if code == 201 else a})")
code, b = svc("np-b", "NodePort", [{"port": 80, "nodePort": 30080}])
check(code == 201 and np(b) == [30080], f"a named node port is kept ({code})")
code, out = svc("np-c", "NodePort", [{"port": 80, "nodePort": 30080}])
check(code == 422 and "provided port is already allocated" in out.get("message", ""), f"the same port again: 422 ({code})")
code, out = svc("np-d", "NodePort", [{"port": 80, "nodePort": 80}])
check(code == 422 and "not in the valid range" in out.get("message", ""), f"out of range: 422 ({code})")
code, out = svc("np-e", "ClusterIP", [{"port": 80, "nodePort": 30081}])
check(code == 422 and "may not be used when `type` is 'ClusterIP'" in out.get("message", ""), f"ClusterIP naming one: 422 ({code})")
code, dns = svc("np-dns", "NodePort", [{"name": "tcp", "port": 53, "protocol": "TCP", "nodePort": 30053},
                                     {"name": "udp", "port": 53, "protocol": "UDP", "nodePort": 30053}])
check(code == 201 and np(dns) == [30053, 30053], f"one number for TCP and UDP ({code})")
code, lb = svc("np-lb", "LoadBalancer", [{"port": 443}])
check(code == 201 and inrange(np(lb)[0]), f"LoadBalancer gets a node port ({code})")

# PUT without the allocated values keeps them.
cur = req("GET", S + "/np-b")[1]
put = json.loads(json.dumps(cur)); put["spec"].pop("clusterIP"); put["spec"].pop("clusterIPs", None)
for p in put["spec"]["ports"]: p.pop("nodePort")
code, out = req("PUT", S + "/np-b", put)
check(code == 200 and np(out) == [30080] and out["spec"]["clusterIP"] == cur["spec"]["clusterIP"], f"PUT leaving them empty keeps them ({code})")
put = req("GET", S + "/np-b")[1]; put["spec"]["clusterIP"] = "10.96.255.250"; put["spec"]["clusterIPs"] = ["10.96.255.250"]
code, out = req("PUT", S + "/np-b", put)
check(code == 422 and "field is immutable" in json.dumps(out), f"a changed clusterIP: 422 ({code})")

# NodePort → ClusterIP frees the port.
cur = req("GET", S + "/np-b")[1]; cur["spec"]["type"] = "ClusterIP"
code, out = req("PUT", S + "/np-b", cur)
check(code == 200 and np(out) == [0] and out["spec"].get("clusterIP"), f"NodePort → ClusterIP drops the node port ({code} {np(out) if code == 200 else out})")
code, _ = svc("np-c", "NodePort", [{"port": 80, "nodePort": 30080}])
check(code == 201, f"…and frees it for another Service ({code})")

# NodePort → ExternalName (conformance), by strategic merge patch.
code, out = req("PATCH", S + "/np-a", {"spec": {"type": "ExternalName", "externalName": "foo.example.com", "clusterIP": ""}},
                "application/strategic-merge-patch+json")
check(code == 200 and not out["spec"].get("clusterIP") and np(out) == [0], f"NodePort → ExternalName drops ClusterIP and node port ({code} {out if code != 200 else ''})")
# ExternalName → ClusterIP / NodePort (conformance).
code, en = svc("en-1", "ExternalName", [{"port": 80}], externalName="foo.example.com")
check(code == 201 and not en["spec"].get("clusterIP"), f"ExternalName created without a ClusterIP ({code})")
en["spec"]["type"] = "ClusterIP"; en["spec"].pop("externalName")
code, out = req("PUT", S + "/en-1", en)
check(code == 200 and out["spec"].get("clusterIP"), f"ExternalName → ClusterIP gets a ClusterIP ({code} {out.get('spec', {}).get('clusterIP') if code == 200 else out})")
code, en2 = svc("en-2", "ExternalName", [{"port": 80}], externalName="foo.example.com")
code, out = req("PATCH", S + "/en-2", {"spec": {"type": "NodePort", "externalName": None}}, "application/merge-patch+json")
check(code == 200 and out["spec"].get("clusterIP") and inrange(np(out)[0]), f"ExternalName → NodePort gets a ClusterIP and a node port ({code})")

# Delete frees the port.
req("DELETE", S + "/np-dns")
code, _ = svc("np-dns2", "NodePort", [{"port": 53, "protocol": "UDP", "nodePort": 30053}])
check(code == 201, f"delete frees the node port ({code})")
sys.exit(failed)
PY
report
