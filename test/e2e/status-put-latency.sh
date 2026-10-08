#!/usr/bin/env bash
#
# Pod status PUT latency and the slow-write breakdown (#191), against a real
# apiserver on fastetcd, idle (no controllers, no scheduler).
#
# - 300 sequential status PUTs of one Pod (the kubelet's write): p50, p99 and
#   max printed; p99 under 50 ms (#191's target on an idle node)
# - /metrics has apiserver_write_phase_duration_seconds for every phase
# - none of those PUTs over 100 ms is missing from the log: every "slow
#   request" for pods/status has its "slow write" breakdown
# - a validating webhook that sleeps 300 ms on pods/status makes one PUT slow:
#   the log has "slow write" for that pod's key with webhooks_ms ≥ 250, and
#   "slow request" for the PUT
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

P=/api/v1/namespaces/default/pods
api() { # <method> <path> [json] — body to $W/out, prints code and seconds
  curl -sk -o "$W/out" -w '%{http_code} %{time_total}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H "Content-Type: application/json" "$API$2" ${3:+-d "$3"}
}
api POST $P '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"lat"},"spec":{"nodeName":"n1","containers":[{"name":"c","image":"pause"}]}}' >/dev/null

python3 - "$API" "$ADMIN" "$W" <<'PY' >"$W/lat"
import json, ssl, sys, time, urllib.request
api, token, W = sys.argv[1:4]
ctx = ssl._create_unverified_context()
url = f"{api}/api/v1/namespaces/default/pods/lat/status"
def call(method, body=None):
    req = urllib.request.Request(url, method=method, data=body,
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, context=ctx) as r:
        return json.loads(r.read())
pod = call("GET"); times = []
for i in range(300):
    pod.setdefault("status", {})["phase"] = "Running"
    pod["status"]["conditions"] = [{"type": "Ready", "status": "True" if i % 2 else "False"}]
    t = time.monotonic(); pod = call("PUT", json.dumps(pod).encode()); times.append((time.monotonic() - t) * 1000)
times.sort()
print(f"{times[len(times)//2]:.1f} {times[int(len(times)*0.99)-1]:.1f} {times[-1]:.1f}")
PY
read -r p50 p99 max <"$W/lat"
echo "status PUT ms: p50 $p50 p99 $p99 max $max"
python3 -c "import sys; sys.exit(0 if float(sys.argv[1]) < 50 else 1)" "${p99:-999}" \
  && pass "status PUT p99 < 50 ms ($p99)" || fail "status PUT p99 < 50 ms (p50 $p50 p99 $p99 max $max)"

m=$(curl -sk -H "Authorization: Bearer $ADMIN" "$API/metrics")
ok=1; for ph in read mutate webhooks write retry_wait; do
  echo "$m" | grep -q "apiserver_write_phase_duration_seconds_count{phase=\"$ph\"}" || ok=0
done
[ $ok = 1 ] && pass "write phase histogram, every phase" || fail "write phase histogram ($(echo "$m" | grep -c write_phase))"

slow_req=$(grep 'slow request' "$W/apiserver.log" | grep -c 'pods/lat/status')
slow_write=$(grep 'slow write' "$W/apiserver.log" | grep -c 'pods/default/lat')
[ "$slow_write" -ge "$slow_req" ] && pass "every slow status PUT has its breakdown ($slow_req slow)" \
  || fail "slow requests $slow_req, breakdowns $slow_write"

# A webhook that takes 300 ms: the breakdown must name it.
WH_PORT=$((PORT + 7))
openssl req -newkey rsa:2048 -nodes -keyout "$W/wh.key" -out "$W/wh.csr" -subj /CN=webhook 2>/dev/null
printf 'subjectAltName=IP:127.0.0.1\n' >"$W/wh.ext"
openssl x509 -req -in "$W/wh.csr" -CA "$W/ca.crt" -CAkey "$W/ca.key" -CAcreateserial \
  -out "$W/wh.crt" -days 2 -extfile "$W/wh.ext" 2>/dev/null
python3 - "$W" "$WH_PORT" <<'PY' >"$W/webhook.log" 2>&1 &
import http.server, json, ssl, sys, time
W, port = sys.argv[1], int(sys.argv[2])
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers["Content-Length"])))["request"]
        time.sleep(0.3)
        body = json.dumps({"apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview",
                           "response": {"uid": req["uid"], "allowed": True}}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
s = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(f"{W}/wh.crt", f"{W}/wh.key")
s.socket = ctx.wrap_socket(s.socket, server_side=True)
print("listening", flush=True); s.serve_forever()
PY
for _ in $(seq 50); do grep -q listening "$W/webhook.log" && break; sleep 0.2; done
api POST /apis/admissionregistration.k8s.io/v1/validatingwebhookconfigurations "{\"apiVersion\":\"admissionregistration.k8s.io/v1\",
  \"kind\":\"ValidatingWebhookConfiguration\",\"metadata\":{\"name\":\"slow\"},\"webhooks\":[{\"name\":\"slow.e2e.io\",
  \"clientConfig\":{\"url\":\"https://127.0.0.1:$WH_PORT/\",\"caBundle\":\"$(openssl base64 -A <"$W/ca.crt")\"},
  \"rules\":[{\"operations\":[\"UPDATE\"],\"apiGroups\":[\"\"],\"apiVersions\":[\"v1\"],\"resources\":[\"pods/status\"]}],
  \"sideEffects\":\"None\",\"admissionReviewVersions\":[\"v1\"],\"failurePolicy\":\"Fail\"}]}" >/dev/null
sleep 2 # the configuration reaches the watch-cache view
pod=$(curl -sk -H "Authorization: Bearer $ADMIN" "$API$P/lat")
body=$(echo "$pod" | python3 -c 'import json,sys; p=json.load(sys.stdin); p["status"]["message"]="slow"; print(json.dumps(p))')
read -r code secs < <(api PUT $P/lat/status "$body")
echo "webhooked PUT: $code in ${secs}s"
line=$(grep 'slow write' "$W/apiserver.log" | grep 'pods/default/lat' | tail -1)
wh=$(echo "$line" | sed -n 's/.*webhooks_ms="\{0,1\}\([0-9.]*\).*/\1/p')
echo "breakdown: $line"
[ "$code" = 200 ] && python3 -c "import sys; sys.exit(0 if float(sys.argv[1]) >= 250 else 1)" "${wh:-0}" \
  && pass "slow write names the webhook time (${wh} ms)" || fail "slow write breakdown ($code, webhooks_ms '${wh}')"
grep 'slow request' "$W/apiserver.log" | grep -q 'pods/lat/status' \
  && pass "slow request logged with its path" || fail "slow request log"
report
