#!/usr/bin/env bash
#
# The VMI control verbs (#141): `PUT …/virtualmachineinstances/{name}/{pause,
# unpause,softreboot,freeze,unfreeze}` proxied to the VMI's kubelet, against
# a real apiserver on fastetcd and a stub kubelet on 127.0.0.1:10250 (TLS,
# self-signed) that records what it is sent and answers like stormvm's router
# via rustkube-node's `/vmVerb` (rustkube-node#94).
#
# - discovery lists the five verbs
# - each verb reaches the stub as `PUT /vmVerb/default/vm/<verb>` with the
#   apiserver's bearer token; the query is passed on; the stub's status and
#   JSON come back (404 from the stub passes through as 404)
# - a dryRun body is answered without calling the node
# - a VMI on no node: 409; a missing VMI: 404
# - the edit role may call them, view may not
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/kubelet.key" -out "$W/kubelet.crt" -days 1 \
  -subj /CN=127.0.0.1 2>/dev/null || exit 100
python3 - "$W" <<'PY' &
import http.server, json, ssl, sys
W = sys.argv[1]
class H(http.server.BaseHTTPRequestHandler):
    def do_PUT(self):
        n = int(self.headers.get("Content-Length") or 0); body = self.rfile.read(n).decode()
        with open(f"{W}/kubelet.log", "a") as f:
            f.write(json.dumps({"path": self.path, "auth": self.headers.get("Authorization", "")[:7], "body": body}) + "\n")
        missing = "/ghost/" in self.path
        out = json.dumps({"error": "vm not registered"} if missing else {"ok": True, "verb": self.path.split("?")[0].rsplit("/", 1)[-1]}).encode()
        self.send_response(404 if missing else 200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out))); self.end_headers(); self.wfile.write(out)
    def log_message(self, *a): pass
srv = http.server.HTTPServer(("127.0.0.1", 10250), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(f"{W}/kubelet.crt", f"{W}/kubelet.key")
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
PY
sleep 1
EDIT=$(token alice '["system:authenticated"]'); VIEW=$(token bob '["system:authenticated"]')
export API ADMIN EDIT VIEW W
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, time, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context(); W = os.environ["W"]
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, token=None, raw=None):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    data = raw if raw is not None else (None if body is None else json.dumps(body))
    c.request(method, path, body=data, headers={"Authorization": "Bearer " + (token or os.environ["ADMIN"]),
                                                "Content-Type": "application/json"})
    r = c.getresponse(); b = r.read(); c.close()
    try: return r.status, json.loads(b)
    except ValueError: return r.status, b
def log():
    try: return [json.loads(l) for l in open(f"{W}/kubelet.log")]
    except FileNotFoundError: return []
req("POST", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions", {
    "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
    "metadata": {"name": "virtualmachineinstances.kubevirt.io"},
    "spec": {"group": "kubevirt.io", "scope": "Namespaced",
             "names": {"plural": "virtualmachineinstances", "singular": "virtualmachineinstance",
                       "kind": "VirtualMachineInstance", "listKind": "VirtualMachineInstanceList"},
             "versions": [{"name": "v1", "served": True, "storage": True, "subresources": {"status": {}},
                           "schema": {"openAPIV3Schema": {"type": "object", "x-kubernetes-preserve-unknown-fields": True}}}]}})
NS = "/apis/kubevirt.io/v1/namespaces/default"
for _ in range(50):
    if req("GET", NS + "/virtualmachineinstances")[0] == 200: break
    time.sleep(0.2)
n = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node", "metadata": {"name": "n1"}})[1]
n["status"] = {"addresses": [{"type": "InternalIP", "address": "127.0.0.1"}]}
req("PUT", "/api/v1/nodes/n1/status", n)
for name, node in [("vm", "n1"), ("ghost", "n1"), ("pending", None)]:
    vmi = req("POST", NS + "/virtualmachineinstances", {"apiVersion": "kubevirt.io/v1", "kind": "VirtualMachineInstance",
                                                        "metadata": {"name": name}, "spec": {}})[1]
    if node:
        vmi["status"] = {"nodeName": node, "phase": "Running"}
        req("PUT", f"{NS}/virtualmachineinstances/{name}/status", vmi)
SUB = "/apis/subresources.kubevirt.io/v1/namespaces/default/virtualmachineinstances"
verbs = ["pause", "unpause", "softreboot", "freeze", "unfreeze"]
res = [r["name"] for r in req("GET", "/apis/subresources.kubevirt.io/v1")[1]["resources"]]
check(all(f"virtualmachineinstances/{v}" in res for v in verbs), f"discovery lists the five verbs ({res})")
for v in verbs:
    q = "?unfreezeTimeout=5m" if v == "freeze" else ""
    code, out = req("PUT", f"{SUB}/vm/{v}{q}", {})
    last = (log() or [{}])[-1]
    check(code == 200 and out == {"ok": True, "verb": v} and last.get("path") == f"/vmVerb/default/vm/{v}{q}"
          and last.get("auth") == "Bearer ", f"{v}: proxied to the kubelet ({code} {out} {last.get('path')})")
before = len(log())
code, out = req("PUT", f"{SUB}/vm/pause", {"dryRun": ["All"]})
check(code == 200 and len(log()) == before, f"dryRun: answered, node not called ({code})")
code, out = req("PUT", f"{SUB}/ghost/pause", {})
check(code == 404, f"the kubelet's 404 passes through ({code} {out})")
code, _ = req("PUT", f"{SUB}/pending/pause", {})
check(code == 409, f"a VMI on no node: 409 ({code})")
code, _ = req("PUT", f"{SUB}/nothere/pause", {})
check(code == 404, f"a missing VMI: 404 ({code})")
req("POST", "/apis/rbac.authorization.k8s.io/v1/namespaces/default/rolebindings", {"apiVersion": "rbac.authorization.k8s.io/v1",
    "kind": "RoleBinding", "metadata": {"name": "alice-edit"}, "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "edit"},
    "subjects": [{"kind": "User", "name": "alice", "apiGroup": "rbac.authorization.k8s.io"}]})
req("POST", "/apis/rbac.authorization.k8s.io/v1/namespaces/default/rolebindings", {"apiVersion": "rbac.authorization.k8s.io/v1",
    "kind": "RoleBinding", "metadata": {"name": "bob-view"}, "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "view"},
    "subjects": [{"kind": "User", "name": "bob", "apiGroup": "rbac.authorization.k8s.io"}]})
time.sleep(1)
code, _ = req("PUT", f"{SUB}/vm/unpause", {}, token=os.environ["EDIT"])
check(code == 200, f"edit may unpause ({code})")
code, _ = req("PUT", f"{SUB}/vm/pause", {}, token=os.environ["VIEW"])
check(code == 403, f"view may not pause ({code})")
sys.exit(failed)
PY
report
