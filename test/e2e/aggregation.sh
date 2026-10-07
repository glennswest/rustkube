#!/usr/bin/env bash
#
# API aggregation (#83) against a real apiserver on a real fastetcd. A Python
# HTTPS server stands in for an aggregated API server (metrics-server's
# shape): group stub.example.com/v1, Service agg/stub with ClusterIP
# 127.0.0.9, its certificate for stub.agg.svc from the rig's CA (the
# APIService's caBundle). It asks for a client certificate from the
# front-proxy CA and logs what each request carried.
#
#   - the APIService goes Available (Passed) on its own; the group is in /apis
#     and /apis/stub.example.com; /apis/stub.example.com/v1 is the backend's
#   - a LIST is proxied: the backend sees the front-proxy certificate,
#     X-Remote-User/Group of the caller, and neither the caller's
#     Authorization nor a forged X-Remote-User
#   - RBAC is this apiserver's: a user without a grant is 403 and the backend
#     never hears of it; one granted widgets is proxied as himself
#   - a POST's body and the backend's 201 pass; a watch streams (the first
#     event arrives before the backend sends the second); a protobuf Accept
#     is passed and the backend's JSON comes back untranscoded
#   - kube-system/extension-apiserver-authentication publishes the
#     front-proxy CA and headers; system:auth-delegator and the reader Role exist
#   - an APIService claiming a built-in group (apps) changes nothing
#   - an APIService for a missing Service is ServiceNotFound
#   - the backend stopped: FailedDiscoveryCheck, and its requests are 503
#
# Exit status is the number of failed checks.
mkdir -p "$PWD/tmp"
FP=$(mktemp -d "$PWD/tmp/aggregation.XXXXXX")
# The front-proxy CA and this apiserver's client certificate from it.
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$FP/fp-ca.key" -out "$FP/fp-ca.crt" -days 2 -subj /CN=front-proxy-ca 2>/dev/null
openssl req -newkey rsa:2048 -nodes -keyout "$FP/proxy.key" -out "$FP/proxy.csr" -subj /CN=front-proxy-client 2>/dev/null
printf 'extendedKeyUsage=clientAuth\n' >"$FP/proxy.ext"
openssl x509 -req -in "$FP/proxy.csr" -CA "$FP/fp-ca.crt" -CAkey "$FP/fp-ca.key" -CAcreateserial \
  -out "$FP/proxy.crt" -days 2 -extfile "$FP/proxy.ext" 2>/dev/null
RK_APISERVER_ARGS="${RK_APISERVER_ARGS:-} --proxy-client-cert-file $FP/proxy.crt --proxy-client-key-file $FP/proxy.key
  --requestheader-client-ca-file $FP/fp-ca.crt --requestheader-allowed-names front-proxy-client"
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
trap 'cleanup; rm -rf "$FP"' EXIT

STUB_PORT=$((PORT + 9))
openssl req -newkey rsa:2048 -nodes -keyout "$W/stub.key" -out "$W/stub.csr" -subj /CN=stub 2>/dev/null
printf 'subjectAltName=DNS:stub.agg.svc\n' >"$W/stub.ext"
openssl x509 -req -in "$W/stub.csr" -CA "$W/ca.crt" -CAkey "$W/ca.key" -CAcreateserial \
  -out "$W/stub.crt" -days 2 -extfile "$W/stub.ext" 2>/dev/null
CA_B64=$(openssl base64 -A <"$W/ca.crt")

python3 - "$W" "$STUB_PORT" "$FP/fp-ca.crt" <<'PY' >"$W/stub.log" 2>&1 &
import http.server, json, ssl, sys, threading, time
W, port, fpca = sys.argv[1], int(sys.argv[2]), sys.argv[3]
lock = threading.Lock()
RES = {"kind": "APIResourceList", "apiVersion": "v1", "groupVersion": "stub.example.com/v1",
       "resources": [{"name": "widgets", "singularName": "widget", "namespaced": True, "kind": "Widget",
                      "verbs": ["get", "list", "watch", "create"]}]}
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def record(self, body=None):
        cert = self.connection.getpeercert() or {}
        cn = [v for rdn in cert.get("subject", ()) for k, v in rdn if k == "commonName"]
        with lock, open(f"{W}/stub.jsonl", "a") as f:
            f.write(json.dumps({"method": self.command, "path": self.path, "cert_cn": cn,
                                "user": self.headers.get("X-Remote-User"), "groups": self.headers.get_all("X-Remote-Group") or [],
                                "authorization": self.headers.get("Authorization"), "accept": self.headers.get("Accept"),
                                "body": body}) + "\n")
    def send(self, code, obj, ctype="application/json"):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", ctype); self.send_header("Content-Length", str(len(b)))
        self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        self.record()
        if self.path.startswith("/apis/stub.example.com/v1/namespaces/default/widgets") and "watch=1" in self.path:
            self.send_response(200); self.send_header("Content-Type", "application/json")
            self.send_header("Transfer-Encoding", "chunked"); self.end_headers()
            for i, delay in ((1, 0), (2, 3)):
                time.sleep(delay)
                line = (json.dumps({"type": "ADDED", "object": {"kind": "Widget", "metadata": {"name": f"w{i}"}}}) + "\n").encode()
                self.wfile.write(b"%x\r\n%s\r\n" % (len(line), line)); self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n"); return
        if self.path.startswith("/apis/stub.example.com/v1/namespaces/default/widgets"):
            return self.send(200, {"kind": "WidgetList", "apiVersion": "stub.example.com/v1", "metadata": {},
                                   "items": [{"metadata": {"name": "w0"}}]})
        if self.path.split("?")[0] == "/apis/stub.example.com/v1":
            return self.send(200, RES)
        self.send(404, {"kind": "Status", "code": 404})
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.record(body)
        body.setdefault("metadata", {})["uid"] = "from-stub"
        self.send(201, body)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(f"{W}/stub.crt", f"{W}/stub.key")
ctx.verify_mode = ssl.CERT_OPTIONAL
ctx.load_verify_locations(fpca)
srv = http.server.ThreadingHTTPServer(("127.0.0.9", port), H)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
PY
STUB=$!

k() { # <method> <path> [json] [token] — body to stdout, code in $W/code
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer ${4:-$ADMIN}" \
    -H 'Content-Type: application/json' "$API$2" ${3:+-d "$3"} >"$W/code"; cat "$W/out"
}
jq_() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1" 2>/dev/null; }
last() { tail -n 1 "$W/stub.jsonl" 2>/dev/null; }
await() { # <what> <python expr over an APIService d> — poll 30 s
  local got
  for _ in $(seq 60); do
    got=$(k GET "/apis/apiregistration.k8s.io/v1/apiservices/$2" | jq_ "$3")
    [ "$got" = True ] && { pass "$1"; return; }
    sleep 0.5
  done
  fail "$1: $(cat "$W/out" | head -c 400)"
}

k POST /api/v1/namespaces '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"agg"}}' >/dev/null
k POST /api/v1/namespaces/agg/services \
  "{\"apiVersion\":\"v1\",\"kind\":\"Service\",\"metadata\":{\"name\":\"stub\"},\"spec\":{\"clusterIP\":\"127.0.0.9\",\"ports\":[{\"port\":$STUB_PORT}]}}" >/dev/null
k POST /apis/apiregistration.k8s.io/v1/apiservices "{\"apiVersion\":\"apiregistration.k8s.io/v1\",\"kind\":\"APIService\",
  \"metadata\":{\"name\":\"v1.stub.example.com\"},\"spec\":{\"group\":\"stub.example.com\",\"version\":\"v1\",
  \"service\":{\"namespace\":\"agg\",\"name\":\"stub\",\"port\":$STUB_PORT},\"caBundle\":\"$CA_B64\",
  \"groupPriorityMinimum\":1000,\"versionPriority\":15}}" >/dev/null
[ "$(cat "$W/code")" = 201 ] && pass "APIService created" || fail "APIService create: $(cat "$W/code") $(cat "$W/out")"
await "the APIService goes Available (Passed)" v1.stub.example.com \
  '[c for c in d["status"]["conditions"] if c["type"]=="Available"][0]["reason"]=="Passed"'

[ "$(k GET /apis | jq_ 'any(g["name"]=="stub.example.com" for g in d["groups"])')" = True ] \
  && pass "/apis lists stub.example.com" || fail "/apis: $(head -c 300 "$W/out")"
[ "$(k GET /apis/stub.example.com | jq_ 'd["kind"]=="APIGroup" and d["preferredVersion"]["version"]=="v1"')" = True ] \
  && pass "/apis/stub.example.com is its APIGroup" || fail "/apis/stub.example.com: $(cat "$W/code") $(head -c 300 "$W/out")"
[ "$(k GET /apis/stub.example.com/v1 | jq_ 'd["resources"][0]["name"]=="widgets"')" = True ] \
  && pass "/apis/stub.example.com/v1 is the backend's resource list" || fail "discovery: $(head -c 300 "$W/out")"

# A LIST as admin, with a forged X-Remote-User.
curl -sk -o "$W/out" -H "Authorization: Bearer $ADMIN" -H 'X-Remote-User: forged' -H 'X-Remote-Group: forged' \
  "$API/apis/stub.example.com/v1/namespaces/default/widgets" >/dev/null
[ "$(jq_ 'd["kind"]=="WidgetList"' <"$W/out")" = True ] && pass "LIST proxied to the backend" || fail "LIST: $(head -c 300 "$W/out")"
L=$(last)
[ "$(echo "$L" | jq_ 'd["cert_cn"]==["front-proxy-client"]')" = True ] \
  && pass "the backend saw the front-proxy client certificate" || fail "front-proxy cert: $L"
[ "$(echo "$L" | jq_ 'd["user"]=="admin" and "system:masters" in d["groups"] and "forged" not in d["groups"]')" = True ] \
  && pass "X-Remote-User/Group are the caller's; the forged ones are dropped" || fail "identity headers: $L"
[ "$(echo "$L" | jq_ 'd["authorization"] is None')" = True ] \
  && pass "the caller's Authorization is not forwarded" || fail "authorization forwarded: $L"

# RBAC: bob has no grant, alice may list widgets.
BOB=$(token bob '[]'); ALICE=$(token alice '[]')
n=$(wc -l <"$W/stub.jsonl")
k GET /apis/stub.example.com/v1/namespaces/default/widgets "" "$BOB" >/dev/null
[ "$(cat "$W/code")" = 403 ] && [ "$(wc -l <"$W/stub.jsonl")" = "$n" ] \
  && pass "no grant: 403 here, the backend never asked" || fail "bob: $(cat "$W/code"), backend lines $n → $(wc -l <"$W/stub.jsonl")"
k POST /apis/rbac.authorization.k8s.io/v1/clusterroles '{"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole",
  "metadata":{"name":"widgets"},"rules":[{"apiGroups":["stub.example.com"],"resources":["widgets"],"verbs":["list","create","watch"]}]}' >/dev/null
k POST /apis/rbac.authorization.k8s.io/v1/clusterrolebindings '{"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRoleBinding",
  "metadata":{"name":"alice-widgets"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"widgets"},
  "subjects":[{"kind":"User","name":"alice","apiGroup":"rbac.authorization.k8s.io"}]}' >/dev/null
sleep 1
k GET /apis/stub.example.com/v1/namespaces/default/widgets "" "$ALICE" >/dev/null
[ "$(cat "$W/code")" = 200 ] && [ "$(last | jq_ 'd["user"]=="alice" and "system:authenticated" in d["groups"]')" = True ] \
  && pass "granted: proxied as alice" || fail "alice: $(cat "$W/code") $(last)"

# POST, watch, protobuf Accept.
k POST /apis/stub.example.com/v1/namespaces/default/widgets '{"kind":"Widget","metadata":{"name":"w9"},"spec":{"size":3}}' >/dev/null
[ "$(cat "$W/code")" = 201 ] && [ "$(jq_ 'd["metadata"]["uid"]=="from-stub"' <"$W/out")" = True ] \
  && [ "$(last | jq_ 'd["body"]["spec"]["size"]==3')" = True ] \
  && pass "POST: body to the backend, its 201 back" || fail "POST: $(cat "$W/code") $(head -c 200 "$W/out")"
python3 - "$API" "$ADMIN" <<'PY' && pass "a watch streams: the first event before the backend sends the second" || fail "watch did not stream"
import http.client, ssl, sys, time, urllib.parse
u = urllib.parse.urlparse(sys.argv[1])
c = http.client.HTTPSConnection(u.hostname, u.port, context=ssl._create_unverified_context(), timeout=20)
t0 = time.time()
c.request("GET", "/apis/stub.example.com/v1/namespaces/default/widgets?watch=1", headers={"Authorization": "Bearer " + sys.argv[2]})
r = c.getresponse(); first = r.fp.readline(); t1 = time.time(); second = r.fp.readline(); t2 = time.time()
ok = r.status == 200 and b"w1" in first and b"w2" in second and t1 - t0 < 2 and t2 - t0 >= 2.5
print(r.status, first[:80], second[:80], round(t1 - t0, 2), round(t2 - t0, 2))
sys.exit(0 if ok else 1)
PY
ct=$(curl -sk -o "$W/out" -w '%{content_type}' -H "Authorization: Bearer $ADMIN" \
  -H 'Accept: application/vnd.kubernetes.protobuf, application/json' "$API/apis/stub.example.com/v1/namespaces/default/widgets")
[ "$ct" = application/json ] && [ "$(jq_ 'd["kind"]=="WidgetList"' <"$W/out")" = True ] \
  && [ "$(last | jq_ '"protobuf" in (d["accept"] or "")')" = True ] \
  && pass "protobuf Accept passed; the backend's JSON comes back as it sent it" || fail "protobuf: $ct $(head -c 100 "$W/out")"

# The front-proxy contract and the roles aggregated servers bind to.
[ "$(k GET /api/v1/namespaces/kube-system/configmaps/extension-apiserver-authentication \
   | jq_ '("BEGIN CERTIFICATE" in d["data"]["requestheader-client-ca-file"] and d["data"]["requestheader-allowed-names"]=="[\"front-proxy-client\"]" and d["data"]["requestheader-username-headers"]=="[\"X-Remote-User\"]")')" = True ] \
  && pass "extension-apiserver-authentication publishes the front-proxy contract" || fail "configmap: $(head -c 300 "$W/out")"
k GET /apis/rbac.authorization.k8s.io/v1/clusterroles/system:auth-delegator >/dev/null; c1=$(cat "$W/code")
k GET /apis/rbac.authorization.k8s.io/v1/namespaces/kube-system/roles/extension-apiserver-authentication-reader >/dev/null; c2=$(cat "$W/code")
[ "$c1$c2" = 200200 ] && pass "system:auth-delegator and extension-apiserver-authentication-reader exist" || fail "roles: $c1 $c2"

# A built-in group cannot be taken over; a missing Service is reported.
k POST /apis/apiregistration.k8s.io/v1/apiservices "{\"apiVersion\":\"apiregistration.k8s.io/v1\",\"kind\":\"APIService\",
  \"metadata\":{\"name\":\"v1.apps\"},\"spec\":{\"group\":\"apps\",\"version\":\"v1\",
  \"service\":{\"namespace\":\"agg\",\"name\":\"stub\",\"port\":$STUB_PORT},\"caBundle\":\"$CA_B64\",\"groupPriorityMinimum\":1,\"versionPriority\":1}}" >/dev/null
sleep 2
[ "$(k GET /apis/apps/v1/namespaces/default/deployments | jq_ 'd["kind"]=="DeploymentList"')" = True ] \
  && pass "an APIService for apps/v1 changes nothing: deployments are local" || fail "apps: $(head -c 200 "$W/out")"
k POST /apis/apiregistration.k8s.io/v1/apiservices '{"apiVersion":"apiregistration.k8s.io/v1","kind":"APIService",
  "metadata":{"name":"v1.gone.example.com"},"spec":{"group":"gone.example.com","version":"v1",
  "service":{"namespace":"agg","name":"nope","port":443},"groupPriorityMinimum":1,"versionPriority":1}}' >/dev/null
await "a missing Service: ServiceNotFound" v1.gone.example.com \
  '[c for c in d["status"]["conditions"] if c["type"]=="Available"][0]["reason"]=="ServiceNotFound"'
k GET /apis/gone.example.com/v1/things >/dev/null
[ "$(cat "$W/code")" = 503 ] && pass "its requests: 503" || fail "unavailable APIService request: $(cat "$W/code")"

# The backend goes away.
kill "$STUB"; wait "$STUB" 2>/dev/null
await "backend stopped: FailedDiscoveryCheck" v1.stub.example.com \
  '[c for c in d["status"]["conditions"] if c["type"]=="Available"][0]["reason"]=="FailedDiscoveryCheck"'
k GET /apis/stub.example.com/v1/namespaces/default/widgets >/dev/null
[ "$(cat "$W/code")" = 503 ] && pass "…and its requests are 503" || fail "after stop: $(cat "$W/code")"
report
