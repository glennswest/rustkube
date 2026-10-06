#!/usr/bin/env bash
#
# Admission webhooks (#82) against a real apiserver on a real fastetcd. A
# Python HTTPS webhook (cert from the rig's CA, sent as caBundle) logs every
# AdmissionReview it is sent and answers by its path:
#
#   /deny     refuses, 403 "no thanks"
#   /mutate   allows with a JSONPatch adding label mutated=yes, and a warning
#   /allow    allows
#
#   - validating, objectSelector deny=yes on ConfigMap CREATE: a labelled
#     create is refused with upstream's message and code, nothing stored; an
#     unlabelled one is stored and never reaches the webhook
#   - mutating, namespaceSelector wh=on, reached through a Service (fixed
#     ClusterIP 127.0.0.7, TLS checked for wh.webhooks.svc): the create in
#     that namespace is stored patched, with the Warning header; the review
#     names the user, operation, kind and resource; elsewhere untouched
#   - UPDATE by PATCH carries oldObject; `nodes/status` PUT is admitted as
#     subResource status (scope Cluster) and refused, the main resource not
#   - DELETE: a Secret named keep is refused on DELETE and in a
#     deletecollection; the others go
#   - failurePolicy: an unreachable webhook refuses under Fail (500 "failed
#     calling webhook"), is passed over under Ignore
#   - an unreachable Fail webhook on admissionregistration objects does not
#     stop its own configuration being deleted
#   - after the configurations are deleted, the refused create is stored
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

WH_PORT=$((PORT + 7))
openssl req -newkey rsa:2048 -nodes -keyout "$W/wh.key" -out "$W/wh.csr" -subj /CN=webhook 2>/dev/null
printf 'subjectAltName=IP:127.0.0.1,DNS:wh.webhooks.svc\n' >"$W/wh.ext"
openssl x509 -req -in "$W/wh.csr" -CA "$W/ca.crt" -CAkey "$W/ca.key" -CAcreateserial \
  -out "$W/wh.crt" -days 2 -extfile "$W/wh.ext" 2>/dev/null
CA_B64=$(openssl base64 -A <"$W/ca.crt")

# The webhook: one server on 127.0.0.1 and one on 127.0.0.7 (the Service's
# ClusterIP), both logging to $W/reviews.jsonl.
python3 - "$W" "$WH_PORT" <<'PY' >"$W/webhook.log" 2>&1 &
import base64, http.server, json, ssl, sys, threading
W, port = sys.argv[1], int(sys.argv[2])
lock = threading.Lock()
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        review = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        req = review["request"]
        with lock, open(f"{W}/reviews.jsonl", "a") as f:
            f.write(json.dumps({"path": self.path, "host": self.headers.get("Host"), "request": req}) + "\n")
        resp = {"uid": req["uid"], "allowed": True}
        if self.path == "/deny":
            resp = {"uid": req["uid"], "allowed": False, "status": {"code": 403, "message": "no thanks"}}
        elif self.path == "/mutate":
            op = [{"op": "add", "path": "/metadata/labels/mutated", "value": "yes"}]
            if not (req["object"].get("metadata") or {}).get("labels"):
                op = [{"op": "add", "path": "/metadata/labels", "value": {"mutated": "yes"}}]
            resp["patchType"] = "JSONPatch"
            resp["patch"] = base64.b64encode(json.dumps(op).encode()).decode()
            resp["warnings"] = ["mutated by the e2e webhook"]
        body = json.dumps({"apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview", "response": resp}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(f"{W}/wh.crt", f"{W}/wh.key")
for host in ("127.0.0.1", "127.0.0.7"):
    s = http.server.ThreadingHTTPServer((host, port), H)
    s.socket = ctx.wrap_socket(s.socket, server_side=True)
    threading.Thread(target=s.serve_forever, daemon=True).start()
print("listening", flush=True)
threading.Event().wait()
PY
for _ in $(seq 50); do grep -q listening "$W/webhook.log" && break; sleep 0.2; done
grep -q listening "$W/webhook.log" || { cat "$W/webhook.log"; exit 100; }

k() { # <method> <path> [json] [content-type] — body in $W/out, headers in $W/hdr, prints the code
  curl -sk -o "$W/out" -D "$W/hdr" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H "Content-Type: ${4:-application/json}" "$API$2" ${3:+-d "$3"}
}
reviews() { [ -f "$W/reviews.jsonl" ] && cat "$W/reviews.jsonl"; }
calls() { reviews | grep -c "\"path\": \"$1\"" ; }
ns() { k POST /api/v1/namespaces "{\"apiVersion\":\"v1\",\"kind\":\"Namespace\",\"metadata\":{\"name\":\"$1\",\"labels\":$2}}" >/dev/null; }
cm() { # <ns> <name> [labels-json]
  local labels=${3:-}; [ -n "$labels" ] || labels='{}'
  k POST "/api/v1/namespaces/$1/configmaps" \
    "{\"apiVersion\":\"v1\",\"kind\":\"ConfigMap\",\"metadata\":{\"name\":\"$2\",\"labels\":$labels},\"data\":{\"a\":\"1\"}}"
}
hook() { # <name> <path> <rules-json> [extra webhook fields] — clientConfig.url to the webhook
  printf '{"name":"%s","clientConfig":{"url":"https://127.0.0.1:%s%s","caBundle":"%s"},"rules":%s,"sideEffects":"None","admissionReviewVersions":["v1"]%s}' \
    "$1" "$WH_PORT" "$2" "$CA_B64" "$3" "${4:-}"
}
config() { # <validating|mutating> <name> <webhooks-json>
  local kind=ValidatingWebhookConfiguration plural=validatingwebhookconfigurations
  [ "$1" = mutating ] && { kind=MutatingWebhookConfiguration; plural=mutatingwebhookconfigurations; }
  local c; c=$(k POST "/apis/admissionregistration.k8s.io/v1/$plural" \
    "{\"apiVersion\":\"admissionregistration.k8s.io/v1\",\"kind\":\"$kind\",\"metadata\":{\"name\":\"$2\"},\"webhooks\":$3}")
  [ "$c" = 201 ] || { echo "setup: $kind $2: $c $(cat "$W/out")"; exit 100; }
}
# A configuration applies to the next write once the watch cache has it
# (milliseconds); give it a moment rather than racing it.
settle() { sleep 1; }

ns wh-off '{}'; ns wh-on '{"wh":"on"}'; ns webhooks '{}'
k POST /api/v1/namespaces/webhooks/services \
  "{\"apiVersion\":\"v1\",\"kind\":\"Service\",\"metadata\":{\"name\":\"wh\"},\"spec\":{\"clusterIP\":\"127.0.0.7\",\"ports\":[{\"port\":$WH_PORT}]}}" >/dev/null

CM_CREATE='[{"operations":["CREATE"],"apiGroups":[""],"apiVersions":["v1"],"resources":["configmaps"]}]'
config validating deny-labelled "[$(hook deny.e2e.rustkube.io /deny "$CM_CREATE" ',"objectSelector":{"matchLabels":{"deny":"yes"}}')]"
CM_WRITE='[{"operations":["CREATE","UPDATE"],"apiGroups":[""],"apiVersions":["v1"],"resources":["configmaps"]}]'
config mutating mutate-wh-on "[{\"name\":\"mutate.e2e.rustkube.io\",\"clientConfig\":{\"service\":{\"namespace\":\"webhooks\",\"name\":\"wh\",\"port\":$WH_PORT,\"path\":\"/mutate\"},\"caBundle\":\"$CA_B64\"},\"rules\":$CM_WRITE,\"namespaceSelector\":{\"matchLabels\":{\"wh\":\"on\"}},\"sideEffects\":\"None\",\"admissionReviewVersions\":[\"v1\"]}]"
settle

# --- validating: refusal, objectSelector ---------------------------------------
c=$(cm wh-off refused '{"deny":"yes"}')
if [ "$c" = 403 ] && grep -q 'admission webhook \\"deny.e2e.rustkube.io\\" denied the request: no thanks' "$W/out"; then
  pass "labelled create refused: 403, upstream's message"
else fail "labelled create: $c $(cat "$W/out")"; fi
c=$(k GET /api/v1/namespaces/wh-off/configmaps/refused); [ "$c" = 404 ] && pass "refused ConfigMap not stored" || fail "refused ConfigMap: GET $c"
before=$(calls /deny)
c=$(cm wh-off plain); [ "$c" = 201 ] && pass "unlabelled create stored (201)" || fail "unlabelled create: $c $(cat "$W/out")"
[ "$(calls /deny)" = "$before" ] && pass "objectSelector: unlabelled create never sent" || fail "deny webhook called for an unlabelled object"

# --- mutating through a Service, namespaceSelector ------------------------------
c=$(cm wh-on patched)
if [ "$c" = 201 ] && python3 -c 'import json,sys; o=json.load(open(sys.argv[1])); sys.exit(o["metadata"]["labels"].get("mutated")!="yes")' "$W/out"; then
  pass "create in wh=on namespace stored with the webhook's patch"
else fail "mutated create: $c $(cat "$W/out")"; fi
grep -qi '^warning: 299 - "mutated by the e2e webhook"' "$W/hdr" && pass "webhook warning returned as a Warning header" \
  || fail "no Warning header: $(cat "$W/hdr")"
last=$(reviews | grep '"/mutate"' | tail -1)
if python3 - "$last" <<'PY'
import json, sys
r = json.loads(sys.argv[1]); q = r["request"]
ok = (r["host"].startswith("wh.webhooks.svc") and q["operation"] == "CREATE"
      and q["userInfo"]["username"] == "admin" and "system:masters" in q["userInfo"]["groups"]
      and q["kind"] == {"group": "", "version": "v1", "kind": "ConfigMap"}
      and q["resource"] == {"group": "", "version": "v1", "resource": "configmaps"}
      and q["namespace"] == "wh-on" and q["name"] == "patched" and q["oldObject"] is None
      and q["object"]["data"] == {"a": "1"} and q["options"]["kind"] == "CreateOptions")
sys.exit(0 if ok else 1)
PY
then pass "review via Service (Host wh.webhooks.svc): user, CREATE, kind, resource, name, namespace"
else fail "review fields: $last"; fi
c=$(k GET /api/v1/namespaces/wh-off/configmaps/plain)
grep -q '"mutated"' "$W/out" && fail "namespaceSelector: wh-off ConfigMap was mutated" || pass "namespaceSelector: wh-off ConfigMap untouched"

# --- UPDATE by PATCH: oldObject --------------------------------------------------
c=$(k PATCH /api/v1/namespaces/wh-on/configmaps/patched '{"data":{"a":"2"}}' application/merge-patch+json)
last=$(reviews | grep '"/mutate"' | tail -1)
if [ "$c" = 200 ] && python3 - "$last" <<'PY'
import json, sys
q = json.loads(sys.argv[1])["request"]
sys.exit(0 if q["operation"] == "UPDATE" and q["oldObject"]["data"] == {"a": "1"}
         and q["object"]["data"] == {"a": "2"} and q["options"]["kind"] == "UpdateOptions" else 1)
PY
then pass "PATCH admitted as UPDATE with oldObject"
else fail "PATCH: $c $(cat "$W/out") review: $last"; fi

# --- /status as a subresource, Cluster scope -------------------------------------
NODE='{"apiVersion":"v1","kind":"Node","metadata":{"name":"wh-node"},"status":{"capacity":{"pods":"10"}}}'
k POST /api/v1/nodes "$NODE" >/dev/null
config validating deny-node-status "[$(hook status.e2e.rustkube.io /deny '[{"operations":["UPDATE"],"apiGroups":[""],"apiVersions":["v1"],"resources":["nodes/status"],"scope":"Cluster"}]')]"
settle
c=$(k PUT /api/v1/nodes/wh-node/status '{"apiVersion":"v1","kind":"Node","metadata":{"name":"wh-node"},"status":{"capacity":{"pods":"20"}}}')
last=$(reviews | grep '"/deny"' | tail -1)
if [ "$c" = 403 ] && echo "$last" | grep -q '"subResource": "status"'; then
  pass "nodes/status PUT refused, review has subResource status"
else fail "nodes/status PUT: $c $(cat "$W/out")"; fi
c=$(k PATCH /api/v1/nodes/wh-node '{"metadata":{"labels":{"x":"y"}}}' application/merge-patch+json)
[ "$c" = 200 ] && pass "nodes (main resource) PATCH not matched by nodes/status" || fail "node PATCH: $c $(cat "$W/out")"

# --- DELETE and deletecollection -------------------------------------------------
for s in keep drop; do
  k POST /api/v1/namespaces/wh-off/secrets "{\"apiVersion\":\"v1\",\"kind\":\"Secret\",\"metadata\":{\"name\":\"$s\",\"labels\":{\"set\":\"x\"}}}" >/dev/null
done
config validating deny-keep-delete "[$(hook delete.e2e.rustkube.io /deny '[{"operations":["DELETE"],"apiGroups":[""],"apiVersions":["v1"],"resources":["secrets"]}]' ',"objectSelector":{"matchExpressions":[{"key":"keep","operator":"Exists"}]}')]"
k PATCH /api/v1/namespaces/wh-off/secrets/keep '{"metadata":{"labels":{"keep":"1"}}}' application/merge-patch+json >/dev/null
settle
c=$(k DELETE /api/v1/namespaces/wh-off/secrets/keep)
last=$(reviews | grep '"/deny"' | tail -1)
if [ "$c" = 403 ] && echo "$last" | python3 -c 'import json,sys; q=json.load(sys.stdin)["request"]; sys.exit(0 if q["operation"]=="DELETE" and q["object"] is None and q["oldObject"]["metadata"]["name"]=="keep" else 1)'; then
  pass "DELETE refused, review has oldObject and no object"
else fail "DELETE keep: $c $(cat "$W/out")"; fi
c=$(k DELETE "/api/v1/namespaces/wh-off/secrets?labelSelector=set%3Dx")
[ "$c" = 403 ] && pass "deletecollection refused by the item it may not delete" || fail "deletecollection: $c $(cat "$W/out")"
c=$(k GET /api/v1/namespaces/wh-off/secrets/keep); [ "$c" = 200 ] && pass "keep still stored" || fail "keep: GET $c"
c=$(k DELETE /api/v1/namespaces/wh-off/secrets/drop); [ "$c" = 200 ] || [ "$c" = 404 ] && pass "an unprotected Secret deletes" || fail "drop: $c"

# --- failurePolicy ---------------------------------------------------------------
SA_CREATE='[{"operations":["CREATE"],"apiGroups":[""],"apiVersions":["v1"],"resources":["serviceaccounts"]}]'
dead() { printf '{"name":"%s","clientConfig":{"url":"https://127.0.0.1:%s/x","caBundle":"%s"},"rules":%s,"failurePolicy":"%s","timeoutSeconds":2,"namespaceSelector":{"matchLabels":{"fp":"%s"}},"sideEffects":"None","admissionReviewVersions":["v1"]}' \
  "$1" "$((WH_PORT + 1))" "$CA_B64" "$SA_CREATE" "$2" "$3"; }
ns fp-fail '{"fp":"fail"}'; ns fp-ignore '{"fp":"ignore"}'
config validating dead "[$(dead fail.e2e.rustkube.io Fail fail),$(dead ignore.e2e.rustkube.io Ignore ignore)]"
settle
sa() { k POST "/api/v1/namespaces/$1/serviceaccounts" '{"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"s"}}'; }
c=$(sa fp-fail)
if [ "$c" = 500 ] && grep -q 'failed calling webhook \\"fail.e2e.rustkube.io\\"' "$W/out"; then
  pass "unreachable webhook, failurePolicy Fail: 500 failed calling webhook"
else fail "Fail policy: $c $(cat "$W/out")"; fi
c=$(sa fp-ignore); [ "$c" = 201 ] && pass "unreachable webhook, failurePolicy Ignore: created" || fail "Ignore policy: $c $(cat "$W/out")"

# --- admissionregistration objects are never sent ------------------------------
LOCKOUT=$(printf '{"name":"lockout.e2e.rustkube.io","clientConfig":{"url":"https://127.0.0.1:%s/x","caBundle":"%s"},"rules":[{"operations":["*"],"apiGroups":["admissionregistration.k8s.io"],"apiVersions":["*"],"resources":["*"]}],"failurePolicy":"Fail","timeoutSeconds":2,"sideEffects":"None","admissionReviewVersions":["v1"]}' "$((WH_PORT + 1))" "$CA_B64")
config validating lockout "[$LOCKOUT]"
settle
c=$(k DELETE /apis/admissionregistration.k8s.io/v1/validatingwebhookconfigurations/lockout)
[ "$c" = 200 ] && pass "a Fail webhook on webhook configurations cannot block its own removal" || fail "lockout delete: $c $(cat "$W/out")"

# --- configurations removed: back to no webhooks -------------------------------
for v in deny-labelled deny-node-status deny-keep-delete dead; do
  k DELETE "/apis/admissionregistration.k8s.io/v1/validatingwebhookconfigurations/$v" >/dev/null
done
k DELETE /apis/admissionregistration.k8s.io/v1/mutatingwebhookconfigurations/mutate-wh-on >/dev/null
settle
c=$(cm wh-off refused '{"deny":"yes"}'); [ "$c" = 201 ] && pass "configurations deleted: the refused create is stored" || fail "after delete: $c $(cat "$W/out")"
c=$(sa fp-fail); [ "$c" = 201 ] && pass "configurations deleted: unreachable webhook no longer consulted" || fail "after delete SA: $c"

[ "$FAIL" -ne 0 ] && { echo "---- webhook log"; cat "$W/webhook.log"; echo "---- reviews"; reviews | cut -c1-400; }
report
