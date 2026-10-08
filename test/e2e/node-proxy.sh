#!/usr/bin/env bash
#
# nodes/{name}/proxy (#108), against a real apiserver on fastetcd and a stub
# kubelet on 127.0.0.1:10250 (TLS, self-signed) that records what it is sent.
#
# - GET …/nodes/n1/proxy/logs/journal?boot=0 reaches the kubelet as
#   GET /logs/journal?boot=0 with the apiserver's bearer token; its status,
#   content type and body come back (`oc adm node-logs`' request)
# - `kubectl get --raw …/proxy/stats/summary` returns the kubelet's JSON
# - POST passes its body and content type; a kubelet 404 passes through
# - discovery lists nodes/proxy; a user without `nodes/proxy` is 403, one
#   granted it may GET; an unknown node is 404
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/kubelet.key" -out "$W/kubelet.crt" -days 1 -subj /CN=127.0.0.1 2>/dev/null || exit 100
python3 - "$W" <<'PY' &
import http.server, json, ssl, sys
W = sys.argv[1]
class H(http.server.BaseHTTPRequestHandler):
    def handle_any(self):
        n = int(self.headers.get("Content-Length") or 0); body = self.rfile.read(n).decode()
        with open(f"{W}/kubelet.log", "a") as f:
            f.write(json.dumps({"method": self.command, "path": self.path, "auth": self.headers.get("Authorization", "")[:7],
                                "ctype": self.headers.get("Content-Type"), "body": body}) + "\n")
        if self.path.startswith("/missing"):
            out, code, ct = b"404 page not found\n", 404, "text/plain"
        elif self.path.startswith("/stats/summary"):
            out, code, ct = json.dumps({"node": {"nodeName": "n1"}}).encode(), 200, "application/json"
        else:
            out, code, ct = f"journal for {self.path}\n".encode(), 200, "text/plain; charset=utf-8"
        self.send_response(code); self.send_header("Content-Type", ct); self.send_header("Content-Length", str(len(out)))
        self.end_headers(); self.wfile.write(out)
    do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = handle_any
    def log_message(self, *a): pass
srv = http.server.HTTPServer(("127.0.0.1", 10250), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(f"{W}/kubelet.crt", f"{W}/kubelet.key")
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
PY
sleep 1
KUBECTL=${RK_TOOLS:+$RK_TOOLS/kubectl}
[ -x "${KUBECTL:-}" ] || KUBECTL=$(command -v kubectl || true)
BOB=$(token bob '["system:authenticated"]'); CAROL=$(token carol '["system:authenticated"]')
export API ADMIN W BOB CAROL KUBECTL
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, subprocess, sys, time, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context(); W = os.environ["W"]
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, token=None, ctype="application/json"):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=body, headers={"Authorization": "Bearer " + (token or os.environ["ADMIN"]), "Content-Type": ctype})
    r = c.getresponse(); raw = r.read(); ct = r.getheader("Content-Type", ""); c.close()
    return r.status, raw.decode(errors="replace"), ct
last = lambda: [json.loads(l) for l in open(f"{W}/kubelet.log")][-1]
n = json.loads(req("POST", "/api/v1/nodes", json.dumps({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "n1"}}))[1])
n["status"] = {"addresses": [{"type": "InternalIP", "address": "127.0.0.1"}]}
req("PUT", "/api/v1/nodes/n1/status", json.dumps(n))
res = json.loads(req("GET", "/api/v1")[1])["resources"]
check(any(r["name"] == "nodes/proxy" for r in res), "discovery lists nodes/proxy")
code, body, ct = req("GET", "/api/v1/nodes/n1/proxy/logs/journal?boot=0")
l = last()
check(code == 200 and body == "journal for /logs/journal?boot=0\n" and ct.startswith("text/plain")
      and l["method"] == "GET" and l["path"] == "/logs/journal?boot=0" and l["auth"] == "Bearer ",
      f"GET proxy/logs/journal reaches the kubelet as /logs/journal ({code} {body!r} {l})")
if os.environ.get("KUBECTL"):
    out = subprocess.run([os.environ["KUBECTL"], "--server", os.environ["API"], "--insecure-skip-tls-verify", "--token", os.environ["ADMIN"],
                          "get", "--raw", "/api/v1/nodes/n1/proxy/stats/summary"], capture_output=True, text=True).stdout
    check('"nodeName": "n1"' in out or '"nodeName":"n1"' in out, f"kubectl get --raw …/proxy/stats/summary ({out[:80]!r})")
code, body, _ = req("POST", "/api/v1/nodes/n1/proxy/run", body='{"x":1}')
l = last()
check(code == 200 and l["method"] == "POST" and l["body"] == '{"x":1}' and l["ctype"] == "application/json", f"POST passes body and content type ({l})")
code, body, _ = req("GET", "/api/v1/nodes/n1/proxy/missing")
check(code == 404 and "404 page not found" in body, f"the kubelet's 404 passes through ({code})")
code, _, _ = req("GET", "/api/v1/nodes/nope/proxy/logs/")
check(code == 404, f"an unknown node: 404 ({code})")
code, _, _ = req("GET", "/api/v1/nodes/n1/proxy/logs/", token=os.environ["BOB"])
check(code == 403, f"without nodes/proxy: 403 ({code})")
req("POST", "/apis/rbac.authorization.k8s.io/v1/clusterroles", json.dumps({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
    "metadata": {"name": "node-proxier"}, "rules": [{"apiGroups": [""], "resources": ["nodes/proxy"], "verbs": ["get"]}]}))
req("POST", "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings", json.dumps({"apiVersion": "rbac.authorization.k8s.io/v1",
    "kind": "ClusterRoleBinding", "metadata": {"name": "carol-proxier"},
    "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "node-proxier"},
    "subjects": [{"kind": "User", "name": "carol", "apiGroup": "rbac.authorization.k8s.io"}]}))
time.sleep(1)
code, _, _ = req("GET", "/api/v1/nodes/n1/proxy/logs/", token=os.environ["CAROL"])
check(code == 200, f"granted nodes/proxy get: 200 ({code})")
sys.exit(failed)
PY
report
