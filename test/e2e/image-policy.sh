#!/usr/bin/env bash
#
# Image policy admission (#385, sc#162) against a real apiserver and
# controller-manager on a real fastetcd, with `oc set image-lookup`. A Python
# HTTPS server stands in for the registry's `image.openshift.io/v1`
# (stormblock-registry#258's shape): ImageStreams `mystream` (tag latest) and
# `other` (tag v1) in namespace img, GET and PATCH; ImageStreamTags GET. It is
# the APIService v1.image.openshift.io, Service img/registry at 127.0.0.9,
# exactly as the aggregation rig stands its stub up.
#
#   - aggregated discovery lists imagestreams (Current), so oc maps them
#   - `oc set image-lookup mystream` sets lookupPolicy.local on the stream;
#     a pod with `image: mystream:latest` is stored as the registry's
#     `…/img/mystream@sha256:…`; the registry heard it from system:apiserver
#   - `oc set image-lookup deploy/web` (stream `other` has no local lookup):
#     the Deployment's template image is resolved, and its new pods run it
#   - `--enabled=false` on the stream: a new pod keeps `mystream:latest`;
#     on the Deployment: the annotation goes, a new `other:v1` stays
#   - a full reference, and a pod with a controller owner, are left alone
#   - the registry stopped: a pod is created at once with its image as written
#     (never refused), both before and after the APIService goes unavailable
#
# Exit status is the number of failed checks.
mkdir -p "$PWD/tmp"
FP=$(mktemp -d "$PWD/tmp/image-policy.XXXXXX")
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
start_controller_manager

if command -v oc >/dev/null; then OC=$(command -v oc); else
  curl -sfL https://mirror.openshift.com/pub/openshift-v4/x86_64/clients/ocp/stable/openshift-client-linux.tar.gz \
    | tar -xz -C "$W" oc || exit 100
  OC=$W/oc
fi
oc_() { KUBECONFIG=$W/kc timeout 60 "$OC" --cache-dir "$W/occache" --server "$API" --insecure-skip-tls-verify \
  --token "$ADMIN" -n img "$@" 2>&1; }

REG_PORT=$((PORT + 9))
openssl req -newkey rsa:2048 -nodes -keyout "$W/reg.key" -out "$W/reg.csr" -subj /CN=registry 2>/dev/null
printf 'subjectAltName=DNS:registry.img.svc\n' >"$W/reg.ext"
openssl x509 -req -in "$W/reg.csr" -CA "$W/ca.crt" -CAkey "$W/ca.key" -CAcreateserial \
  -out "$W/reg.crt" -days 2 -extfile "$W/reg.ext" 2>/dev/null
CA_B64=$(openssl base64 -A <"$W/ca.crt")
DIGEST=sha256:$(printf '%064d' 7)
HOST=node1.lo:5000

python3 - "$W" "$REG_PORT" "$FP/fp-ca.crt" "$DIGEST" "$HOST" <<'PY' >"$W/reg.log" 2>&1 &
import http.server, json, ssl, sys, threading
W, port, fpca, digest, host = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]
lock = threading.Lock()
G = "/apis/image.openshift.io/v1"
RES = {"kind": "APIResourceList", "apiVersion": "v1", "groupVersion": "image.openshift.io/v1", "resources": [
    {"name": "imagestreams", "singularName": "imagestream", "namespaced": True, "kind": "ImageStream",
     "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"], "shortNames": ["is"]},
    {"name": "imagestreams/status", "singularName": "", "namespaced": True, "kind": "ImageStream", "verbs": ["get", "update"]},
    {"name": "imagestreamtags", "singularName": "imagestreamtag", "namespaced": True, "kind": "ImageStreamTag",
     "verbs": ["create", "delete", "get", "list", "update"], "shortNames": ["istag"]}]}
streams = {}
for name, tag in (("mystream", "latest"), ("other", "v1")):
    streams[name] = {"kind": "ImageStream", "apiVersion": "image.openshift.io/v1",
        "metadata": {"name": name, "namespace": "img", "uid": "uid-" + name, "resourceVersion": "1",
                     "creationTimestamp": "2026-11-25T00:00:00Z"},
        "spec": {"lookupPolicy": {"local": False}},
        "status": {"dockerImageRepository": f"{host}/img/{name}",
                   "tags": [{"tag": tag, "items": [{"created": "2026-11-25T00:00:00Z", "image": digest, "generation": 1,
                             "dockerImageReference": f"{host}/img/{name}@{digest}"}]}]}}
def merge(dst, patch):
    for k, v in patch.items():
        if v is None: dst.pop(k, None)
        elif isinstance(v, dict) and isinstance(dst.get(k), dict): merge(dst[k], v)
        else: dst[k] = v
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def record(self):
        with lock, open(f"{W}/reg.jsonl", "a") as f:
            f.write(json.dumps({"method": self.command, "path": self.path, "user": self.headers.get("X-Remote-User"),
                                "groups": self.headers.get_all("X-Remote-Group") or []}) + "\n")
    def send(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def notfound(self, kind, name):
        self.send(404, {"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound", "code": 404,
                        "message": f'{kind}.image.openshift.io "{name}" not found',
                        "details": {"name": name, "group": "image.openshift.io", "kind": kind}})
    def target(self):
        p = self.path.split("?")[0]
        if not p.startswith(G + "/namespaces/img/"): return None, None
        rest = p[len(G + "/namespaces/img/"):].split("/")
        return (rest[0], rest[1]) if len(rest) == 2 else (rest[0], None)
    def do_GET(self):
        self.record()
        if self.path.split("?")[0] == G: return self.send(200, RES)
        res, name = self.target()
        if res == "imagestreams" and name:
            return self.send(200, streams[name]) if name in streams else self.notfound("imagestreams", name)
        if res == "imagestreamtags" and name:
            s, _, tag = name.partition(":")
            st = streams.get(s)
            item = next((t["items"][0] for t in (st or {}).get("status", {}).get("tags", []) if t["tag"] == tag), None)
            if not item: return self.notfound("imagestreamtags", name)
            return self.send(200, {"kind": "ImageStreamTag", "apiVersion": "image.openshift.io/v1",
                "metadata": {"name": name, "namespace": "img"}, "lookupPolicy": st["spec"]["lookupPolicy"],
                "image": {"metadata": {"name": item["image"]}, "dockerImageReference": item["dockerImageReference"]}})
        if res == "imagestreams":
            return self.send(200, {"kind": "ImageStreamList", "apiVersion": "image.openshift.io/v1",
                                   "metadata": {"resourceVersion": "1"}, "items": list(streams.values())})
        self.send(404, {"kind": "Status", "code": 404})
    def do_PATCH(self):
        self.record()
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        res, name = self.target()
        if res != "imagestreams" or name not in streams: return self.notfound("imagestreams", name or "")
        merge(streams[name], body)
        streams[name]["metadata"]["resourceVersion"] = str(int(streams[name]["metadata"]["resourceVersion"]) + 1)
        self.send(200, streams[name])
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(f"{W}/reg.crt", f"{W}/reg.key")
ctx.verify_mode = ssl.CERT_OPTIONAL
ctx.load_verify_locations(fpca)
srv = http.server.ThreadingHTTPServer(("127.0.0.9", port), H)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
PY
REG=$!

k() { # <method> <path> [json] [content-type] — body to stdout, code in $W/code
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H "Content-Type: ${4:-application/json}" "$API$2" ${3:+-d "$3"} >"$W/code"; cat "$W/out"
}
jq_() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1" 2>/dev/null; }
pod() { # <name> <image> [extra metadata json] — the stored pod's first image
  k POST /api/v1/namespaces/img/pods "{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{\"name\":\"$1\"${3:+,$3}},
    \"spec\":{\"containers\":[{\"name\":\"c\",\"image\":\"$2\"}]}}" >/dev/null
  [ "$(cat "$W/code")" = 201 ] || { echo "create $(cat "$W/code") $(head -c 200 "$W/out")"; return; }
  k GET "/api/v1/namespaces/img/pods/$1" | jq_ 'd["spec"]["containers"][0]["image"]'
}
available() { # <reason> — poll 30 s for the APIService's Available reason
  for _ in $(seq 60); do
    [ "$(k GET /apis/apiregistration.k8s.io/v1/apiservices/v1.image.openshift.io \
        | jq_ '[c for c in d["status"]["conditions"] if c["type"]=="Available"][0]["reason"]')" = "$1" ] && return 0
    sleep 0.5
  done
  return 1
}
RESOLVED_MY="$HOST/img/mystream@$DIGEST"
RESOLVED_OTHER="$HOST/img/other@$DIGEST"

k POST /api/v1/namespaces '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"img"}}' >/dev/null
k POST /api/v1/namespaces/img/services \
  "{\"apiVersion\":\"v1\",\"kind\":\"Service\",\"metadata\":{\"name\":\"registry\"},\"spec\":{\"clusterIP\":\"127.0.0.9\",\"ports\":[{\"port\":$REG_PORT}]}}" >/dev/null

# Before the APIService: nothing serves ImageStreams, nothing is looked up.
got=$(pod before mystream:latest)
[ "$got" = mystream:latest ] && pass "no image API: the image is left as written" || fail "no image API: $got"

k POST /apis/apiregistration.k8s.io/v1/apiservices "{\"apiVersion\":\"apiregistration.k8s.io/v1\",\"kind\":\"APIService\",
  \"metadata\":{\"name\":\"v1.image.openshift.io\"},\"spec\":{\"group\":\"image.openshift.io\",\"version\":\"v1\",
  \"service\":{\"namespace\":\"img\",\"name\":\"registry\",\"port\":$REG_PORT},\"caBundle\":\"$CA_B64\",
  \"groupPriorityMinimum\":900,\"versionPriority\":15}}" >/dev/null
available Passed && pass "v1.image.openshift.io goes Available" || fail "APIService: $(head -c 400 "$W/out")"

# Aggregated discovery carries the backend's resources: oc can map them.
curl -sk -o "$W/out" -H "Authorization: Bearer $ADMIN" \
  -H 'Accept: application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList' "$API/apis"
[ "$(jq_ '[v for g in d["items"] if g["metadata"]["name"]=="image.openshift.io" for v in g["versions"]][0]["freshness"]=="Current" and any(r["resource"]=="imagestreams" for g in d["items"] if g["metadata"]["name"]=="image.openshift.io" for r in g["versions"][0]["resources"])' <"$W/out")" = True ] \
  && pass "aggregated discovery lists imagestreams, Current" || fail "aggregated discovery: $(python3 -c 'import json,sys; d=json.load(sys.stdin); print([g for g in d["items"] if g["metadata"]["name"]=="image.openshift.io"])' <"$W/out" | head -c 400)"
out=$(oc_ api-resources --api-group=image.openshift.io)
grep -q '^imagestreams ' <<<"$out" && pass "oc api-resources maps imagestreams" || fail "oc api-resources: $out"

# The stream.
out=$(oc_ set image-lookup mystream)
[ "$(k GET /apis/image.openshift.io/v1/namespaces/img/imagestreams/mystream | jq_ 'd["spec"]["lookupPolicy"]["local"]')" = True ] \
  && pass "oc set image-lookup mystream: lookupPolicy.local (through the aggregator)" || fail "set image-lookup mystream: $out $(head -c 300 "$W/out")"
got=$(pod p1 mystream:latest)
[ "$got" = "$RESOLVED_MY" ] && pass "a pod's mystream:latest is stored as $got" || fail "pod p1: $got"
[ "$(grep imagestreamtags/mystream:latest "$W/reg.jsonl" | tail -1 | jq_ 'd["user"]=="system:apiserver" and "system:masters" in d["groups"]')" = True ] \
  && pass "the registry was asked by system:apiserver" || fail "lookup identity: $(grep imagestreamtags "$W/reg.jsonl" | tail -1)"
got=$(pod p2 mystream)
[ "$got" = "$RESOLVED_MY" ] && pass "no tag means latest" || fail "pod p2: $got"
got=$(pod p3 other:v1)
[ "$got" = other:v1 ] && pass "a stream without local lookup: left" || fail "pod p3: $got"
got=$(pod p4 docker.io/library/mystream:latest)
[ "$got" = docker.io/library/mystream:latest ] && pass "a full reference: left" || fail "pod p4: $got"
got=$(pod p5 mystream:latest '"ownerReferences":[{"apiVersion":"v1","kind":"ReplicationController","name":"rc","uid":"u1","controller":true}]')
[ "$got" = mystream:latest ] && pass "a pod with a controller owner: left" || fail "pod p5: $got"

# The workload.
k POST /apis/apps/v1/namespaces/img/deployments '{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web"},
  "spec":{"replicas":1,"selector":{"matchLabels":{"app":"web"}},"template":{"metadata":{"labels":{"app":"web"}},
  "spec":{"containers":[{"name":"web","image":"other:v1"}]}}}}' >/dev/null
dimage() { k GET /apis/apps/v1/namespaces/img/deployments/web | jq_ 'd["spec"]["template"]["spec"]["containers"][0]["image"]'; }
got=$(dimage)
[ "$got" = other:v1 ] && pass "a Deployment without the annotation: left" || fail "deploy create: $got"
out=$(oc_ set image-lookup deploy/web)
got=$(dimage)
[ "$got" = "$RESOLVED_OTHER" ] && pass "oc set image-lookup deploy/web: the template is resolved" || fail "deploy: $got ($out)"
ok=
for _ in $(seq 60); do
  # No kubelet: the first ReplicaSet's pod never goes away, so the new
  # one's pod is looked for among them.
  imgs=$(k GET '/api/v1/namespaces/img/pods?labelSelector=app%3Dweb' | jq_ '" ".join(sorted(set(p["spec"]["containers"][0]["image"] for p in d["items"])))')
  grep -qF "$RESOLVED_OTHER" <<<"$imgs" && { ok=1; break; }
  sleep 0.5
done
[ -n "$ok" ] && pass "the Deployment's pods run $RESOLVED_OTHER" || fail "deployment pods: $imgs"

# --enabled=false.
out=$(oc_ set image-lookup mystream --enabled=false)
got=$(pod p6 mystream:latest)
[ "$got" = mystream:latest ] && pass "--enabled=false on the stream: a new pod keeps mystream:latest" || fail "p6: $got ($out)"
out=$(oc_ set image-lookup deploy/web --enabled=false)
k PATCH /apis/apps/v1/namespaces/img/deployments/web '{"spec":{"template":{"spec":{"containers":[{"name":"web","image":"other:v1"}]}}}}' \
  application/strategic-merge-patch+json >/dev/null
got=$(dimage)
[ "$got" = other:v1 ] && pass "--enabled=false on the Deployment: a new other:v1 stays" || fail "deploy after disable: $got ($out)"

# The registry goes away: never refused, never resolved.
k PATCH /apis/image.openshift.io/v1/namespaces/img/imagestreams/mystream '{"spec":{"lookupPolicy":{"local":true}}}' \
  application/merge-patch+json >/dev/null
kill "$REG"; wait "$REG" 2>/dev/null
t0=$(date +%s%3N)
got=$(pod down1 mystream:latest)
ms=$(( $(date +%s%3N) - t0 ))
[ "$got" = mystream:latest ] && [ "$ms" -lt 5000 ] && pass "registry down (APIService still Available): created as written in ${ms} ms" \
  || fail "down1: $got in ${ms} ms"
available FailedDiscoveryCheck || fail "the APIService never went unavailable"
got=$(pod down2 mystream:latest)
[ "$got" = mystream:latest ] && pass "registry down (APIService unavailable): created as written" || fail "down2: $got"
report
