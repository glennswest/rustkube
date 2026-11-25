#!/usr/bin/env bash
#
# Image triggers (#386, sc#162) against a real apiserver and controller-manager
# (as itself: its role is what lets it watch streams and tag) on a real
# fastetcd, with `oc set triggers`. A Python HTTPS server stands in for the
# registry's `image.openshift.io/v1` (stormblock-registry#258's shape): LIST
# and WATCH of ImageStreams (one counter, newline-delimited events), GET of a
# stream, PUT/POST of ImageStreamTags, and a `/_push` the script calls to
# move a tag as a push would. `mystream` has no uid, as a stream the registry
# shows only because something was pushed. It is the APIService
# v1.image.openshift.io, Service img/registry at 127.0.0.9, as the
# image-policy rig stands its stub up. No kubelet: no pod becomes ready.
#
#   1. the issue's check: `oc set triggers deploy/web --from-image=mystream:latest
#      -c web` → the container image becomes the tag's digest; a push of a new
#      image → the Deployment's image is the new digest and a new ReplicaSet
#      runs it (it rolls)
#   2. `oc set triggers deploy/web --manual` → a push leaves it; `--auto` → it
#      follows again
#   3. a DeploymentConfig with an ImageChange trigger and an empty image:
#      resolved, version 1 made; its Recreate pre hook's tagImages tags the
#      image into img/deployed:prod (as system:kube-controller-manager); a
#      push → version 2, cause ImageChange; `oc rollout latest` on a manual
#      trigger after a push resolves it (processTriggers)
#   4. a BuildConfig (Docker, from mystream:latest, ConfigChange + ImageChange
#      + Generic + GitHub): its first build's base image is the tag's digest,
#      recorded in status.imageChangeTriggers; a push → a build "Image change"
#      from the new digest; the generic webhook with its secret → a build
#      "Generic WebHook" at the posted commit; a wrong secret → 401; a GitHub
#      ping → 200, no build; `oc set triggers bc/app` lists them
#
# Exit status is the number of failed checks.
mkdir -p "$PWD/tmp"
FP=$(mktemp -d "$PWD/tmp/image-triggers.XXXXXX")
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

if command -v oc >/dev/null; then OC=$(command -v oc); elif [ -x "${RK_TOOLS:-}/oc" ]; then OC=$RK_TOOLS/oc; else
  curl -sfL https://mirror.openshift.com/pub/openshift-v4/x86_64/clients/ocp/stable/openshift-client-linux.tar.gz \
    | tar -xz -C "$W" oc || exit 100
  OC=$W/oc
fi

REG_PORT=$((PORT + 9))
openssl req -newkey rsa:2048 -nodes -keyout "$W/reg.key" -out "$W/reg.csr" -subj /CN=registry 2>/dev/null
printf 'subjectAltName=DNS:registry.img.svc,IP:127.0.0.9\n' >"$W/reg.ext"
openssl x509 -req -in "$W/reg.csr" -CA "$W/ca.crt" -CAkey "$W/ca.key" -CAcreateserial \
  -out "$W/reg.crt" -days 2 -extfile "$W/reg.ext" 2>/dev/null
CA_B64=$(openssl base64 -A <"$W/ca.crt")
HOST=node1.lo:5000

python3 - "$W" "$REG_PORT" "$FP/fp-ca.crt" "$HOST" <<'PY' >"$W/reg.log" 2>&1 &
import http.server, json, ssl, sys, threading, time
W, port, fpca, host = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
cond = threading.Condition()
G = "/apis/image.openshift.io/v1"
RES = {"kind": "APIResourceList", "apiVersion": "v1", "groupVersion": "image.openshift.io/v1", "resources": [
    {"name": "imagestreams", "singularName": "imagestream", "namespaced": True, "kind": "ImageStream",
     "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"], "shortNames": ["is"]},
    {"name": "imagestreamtags", "singularName": "imagestreamtag", "namespaced": True, "kind": "ImageStreamTag",
     "verbs": ["create", "delete", "get", "list", "update"], "shortNames": ["istag"]}]}
state = {"rv": 1}
streams = {}   # name -> stream (namespace img)
events = []    # (rv, event)
def digest(n): return "sha256:" + ("%064d" % n)
def stream(name, uid=True):
    meta = {"name": name, "namespace": "img", "creationTimestamp": "2026-11-25T00:00:00Z"}
    if uid: meta["uid"] = "uid-" + name
    return {"kind": "ImageStream", "apiVersion": "image.openshift.io/v1", "metadata": meta, "spec": {},
            "status": {"dockerImageRepository": f"{host}/img/{name}", "tags": []}}
def bump(name, kind="MODIFIED"):
    state["rv"] += 1
    s = streams[name]; s["metadata"]["resourceVersion"] = str(state["rv"])
    events.append((state["rv"], {"type": kind, "object": json.loads(json.dumps(s))}))
    cond.notify_all()
def set_tag(name, tag, ref, image):
    s = streams.setdefault(name, stream(name))
    tags = s["status"]["tags"]
    t = next((t for t in tags if t["tag"] == tag), None)
    if t is None: t = {"tag": tag, "items": []}; tags.append(t)
    t["items"].insert(0, {"created": "2026-11-25T00:00:00Z", "dockerImageReference": ref, "image": image, "generation": 1})
with cond:
    streams["mystream"] = stream("mystream", uid=False)  # a push-only view: no uid
    set_tag("mystream", "latest", f"{host}/img/mystream@{digest(1)}", digest(1))
    bump("mystream", "ADDED")
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def record(self, body=None):
        with open(f"{W}/reg.jsonl", "a") as f:
            f.write(json.dumps({"method": self.command, "path": self.path, "user": self.headers.get("X-Remote-User"),
                                "body": body}) + "\n")
    def send(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def notfound(self, kind, name):
        self.send(404, {"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound", "code": 404,
                        "message": f'{kind}.image.openshift.io "{name}" not found'})
    def chunk(self, data):
        self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data)); self.wfile.flush()
    def do_GET(self):
        self.record()
        path, _, q = self.path.partition("?")
        args = dict(p.split("=", 1) for p in q.split("&") if "=" in p)
        if path == G: return self.send(200, RES)
        if path == G + "/imagestreams" and args.get("watch") in ("true", "1"):
            since = int(args.get("resourceVersion") or 0)
            deadline = time.time() + min(int(args.get("timeoutSeconds", "60")), 60)
            self.send_response(200); self.send_header("Content-Type", "application/json")
            self.send_header("Transfer-Encoding", "chunked"); self.end_headers()
            try:
                while time.time() < deadline:
                    with cond:
                        out = [e for rv, e in events if rv > since]
                        if not out:
                            cond.wait(timeout=1)
                            continue
                        since = state["rv"]
                    for e in out:
                        self.chunk(json.dumps(e).encode() + b"\n")
                self.chunk(b"")
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        if path == G + "/imagestreams":
            with cond:
                return self.send(200, {"kind": "ImageStreamList", "apiVersion": "image.openshift.io/v1",
                                       "metadata": {"resourceVersion": str(state["rv"])}, "items": list(streams.values())})
        pre = G + "/namespaces/img/imagestreams/"
        if path.startswith(pre):
            name = path[len(pre):]
            with cond:
                return self.send(200, streams[name]) if name in streams else self.notfound("imagestreams", name)
        self.send(404, {"kind": "Status", "code": 404})
    def tag_write(self, create):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.record(body)
        name = body["metadata"]["name"]
        s, _, tag = name.partition(":")
        with cond:
            if not create and s not in streams: return self.notfound("imagestreamtags", name)
            ref = body["tag"]["from"]["name"]
            set_tag(s, tag, ref, ref.split("@")[-1])
            bump(s)
        self.send(201 if create else 200, {"kind": "ImageStreamTag", "apiVersion": "image.openshift.io/v1",
                                           "metadata": {"name": name, "namespace": "img"}, "tag": body["tag"]})
    def do_PUT(self):
        if self.path.startswith(G + "/namespaces/img/imagestreamtags/"): return self.tag_write(False)
        self.send(404, {"kind": "Status", "code": 404})
    def do_POST(self):
        if self.path.split("?")[0] == G + "/namespaces/img/imagestreamtags": return self.tag_write(True)
        if self.path == "/_push":
            b = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            with cond:
                set_tag(b["stream"], b["tag"], f"{host}/img/{b['stream']}@{digest(b['n'])}", digest(b["n"]))
                bump(b["stream"])
            return self.send(200, {})
        self.send(404, {"kind": "Status", "code": 404})
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(f"{W}/reg.crt", f"{W}/reg.key")
ctx.verify_mode = ssl.CERT_OPTIONAL
ctx.load_verify_locations(fpca)
srv = http.server.ThreadingHTTPServer(("127.0.0.9", port), H)
srv.daemon_threads = True
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
PY
REG=$!
export API ADMIN OC W REG_PORT CA_B64 HOST

python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, subprocess, sys, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"]); TOKEN = os.environ["ADMIN"]; OC = os.environ["OC"]; W = os.environ["W"]
HOST, REG_PORT = os.environ["HOST"], int(os.environ["REG_PORT"])
CTX = ssl._create_unverified_context()
NS = "img"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(method, path, body=None, ctype="application/json", raw=None, headers=None):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    h = {"Authorization": "Bearer " + TOKEN, "Content-Type": ctype, **(headers or {})}
    conn.request(method, path, body=raw if raw is not None else (None if body is None else json.dumps(body)), headers=h)
    r = conn.getresponse(); data = r.read()
    try:
        return r.status, json.loads(data)
    except ValueError:
        return r.status, data.decode(errors="replace")

def get(path):
    st, v = req("GET", path)
    return v if st == 200 else None

def until(f, timeout=40):
    end = time.time() + timeout
    while time.time() < end:
        v = f()
        if v:
            return v
        time.sleep(0.5)
    return f()

def oc(*args):
    p = subprocess.run([OC, "--server", os.environ["API"], "--insecure-skip-tls-verify", "--token", TOKEN,
                        "--cache-dir", f"{W}/oc-cache", "-n", NS, *args],
                       capture_output=True, text=True, timeout=120, env={**os.environ, "KUBECONFIG": f"{W}/oc-kc"})
    return p.returncode, p.stdout + p.stderr

def push(n, stream="mystream", tag="latest"):
    conn = http.client.HTTPSConnection("127.0.0.9", REG_PORT, context=CTX, timeout=10)
    conn.request("POST", "/_push", body=json.dumps({"stream": stream, "tag": tag, "n": n}), headers={"Content-Type": "application/json"})
    conn.getresponse().read()

def ref(n, stream="mystream"):
    return f"{HOST}/img/{stream}@sha256:{n:064d}"

def reg_log():
    try:
        return [json.loads(l) for l in open(f"{W}/reg.jsonl")]
    except FileNotFoundError:
        return []

req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": NS}})
req("POST", f"/api/v1/namespaces/{NS}/services", {"apiVersion": "v1", "kind": "Service", "metadata": {"name": "registry"},
    "spec": {"clusterIP": "127.0.0.9", "ports": [{"port": REG_PORT}]}})
req("POST", "/apis/apiregistration.k8s.io/v1/apiservices", {"apiVersion": "apiregistration.k8s.io/v1", "kind": "APIService",
    "metadata": {"name": "v1.image.openshift.io"}, "spec": {"group": "image.openshift.io", "version": "v1",
    "service": {"namespace": NS, "name": "registry", "port": REG_PORT}, "caBundle": os.environ["CA_B64"],
    "groupPriorityMinimum": 900, "versionPriority": 15}})
avail = lambda: any(c["type"] == "Available" and c["status"] == "True"
                    for c in (get("/apis/apiregistration.k8s.io/v1/apiservices/v1.image.openshift.io") or {}).get("status", {}).get("conditions", []))
check(until(avail), "v1.image.openshift.io goes Available")
check(until(lambda: any(e["method"] == "GET" and "/imagestreams?" in e["path"] and "watch=true" in e["path"]
                        and e["user"] == "system:kube-controller-manager" for e in reg_log())),
      "the controller-manager watches ImageStreams through the aggregator, as itself")

# 1. The issue's check.
req("POST", f"/apis/apps/v1/namespaces/{NS}/deployments", {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"},
    "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "web"}}, "template": {"metadata": {"labels": {"app": "web"}},
    "spec": {"containers": [{"name": "web", "image": "busybox"}, {"name": "side", "image": "pause"}]}}}})
code, out = oc("set", "triggers", "deploy/web", "--from-image=mystream:latest", "-c", "web")
dep = lambda: get(f"/apis/apps/v1/namespaces/{NS}/deployments/web") or {}
image = lambda: [c["image"] for c in dep().get("spec", {}).get("template", {}).get("spec", {}).get("containers", [])]
ann = dep().get("metadata", {}).get("annotations", {}).get("image.openshift.io/triggers", "")
check(code == 0 and "mystream:latest" in ann, f"oc set triggers writes the annotation: {code} {out[:200]} {ann}")
check(until(lambda: image() == [ref(1), "pause"]), f"the trigger resolves web to the tag's digest: {image()}")
push(2)
check(until(lambda: image() == [ref(2), "pause"]), f"a push moves the Deployment to the new digest: {image()}")
rss = lambda: [r["spec"]["template"]["spec"]["containers"][0]["image"]
               for r in (get(f"/apis/apps/v1/namespaces/{NS}/replicasets?labelSelector=app%3Dweb") or {}).get("items", [])]
check(until(lambda: ref(2) in rss()), f"and it rolls: a ReplicaSet runs {ref(2)}: {rss()}")

# 2. --manual / --auto.
code, out = oc("set", "triggers", "deploy/web", "--manual")
push(3)
time.sleep(5)
check(code == 0 and image()[0] == ref(2), f"--manual: a push leaves it at {image()[0]} ({out[:120]})")
code, out = oc("set", "triggers", "deploy/web", "--auto")
check(until(lambda: image()[0] == ref(3)), f"--auto: it follows again: {image()[0]}")

# 3. A DeploymentConfig.
st, out = req("POST", f"/apis/apps.openshift.io/v1/namespaces/{NS}/deploymentconfigs", {
    "apiVersion": "apps.openshift.io/v1", "kind": "DeploymentConfig", "metadata": {"name": "app"},
    "spec": {"replicas": 1, "selector": {"app": "app"},
             "strategy": {"type": "Recreate", "recreateParams": {"pre": {"failurePolicy": "Abort",
                          "tagImages": [{"containerName": "web", "to": {"kind": "ImageStreamTag", "name": "deployed:prod"}}]}}},
             "triggers": [{"type": "ConfigChange"},
                          {"type": "ImageChange", "imageChangeParams": {"automatic": True, "containerNames": ["web"],
                           "from": {"kind": "ImageStreamTag", "name": "mystream:latest"}}}],
             "template": {"metadata": {"labels": {"app": "app"}}, "spec": {"containers": [{"name": "web", "image": " "}]}}}})
check(st == 201, f"a DC with an ImageChange trigger and no image yet is admitted: {st} {str(out)[:200]}")
dc = lambda: get(f"/apis/apps.openshift.io/v1/namespaces/{NS}/deploymentconfigs/app") or {}
rc = lambda n: get(f"/api/v1/namespaces/{NS}/replicationcontrollers/app-{n}") or {}
rc_image = lambda n: (rc(n).get("spec", {}).get("template", {}).get("spec", {}).get("containers") or [{}])[0].get("image")
check(until(lambda: rc_image(1) == ref(3)), f"version 1 runs the resolved image: {rc_image(1)}")
check(dc().get("spec", {}).get("triggers", [{}, {}])[1].get("imageChangeParams", {}).get("lastTriggeredImage") == ref(3),
      "lastTriggeredImage is recorded")
tagged = lambda: [e for e in reg_log() if e["method"] in ("PUT", "POST") and "imagestreamtags" in e["path"]
                  and (e["body"] or {}).get("metadata", {}).get("name") == "deployed:prod"]
check(until(lambda: any(e["body"]["tag"]["from"]["name"] == ref(3) and e["user"] == "system:kube-controller-manager" for e in tagged())),
      f"the pre hook's tagImages tags {ref(3)} into deployed:prod: {tagged()[-1:] if tagged() else 'nothing'}")
log = lambda: rc(1).get("metadata", {}).get("annotations", {}).get("deploy.rustkube.io/log", "")
check(until(lambda: "--> pre: Tagged" in log()), f"the rollout's log says so: {log()[-200:]}")
push(4)
check(until(lambda: rc_image(2) == ref(4), timeout=60), f"a push makes version 2 with the new image: {rc_image(2)}")
causes = dc().get("status", {}).get("details", {}).get("causes", [{}])
check(causes[0].get("type") == "ImageChange", f"cause ImageChange: {causes}")
# A manual trigger: a push waits for oc rollout latest.
code, out = oc("set", "triggers", "dc/app", "--manual")
push(5)
time.sleep(5)
v = dc().get("status", {}).get("latestVersion")
check(v == 2, f"--manual: a push makes no version (latestVersion {v})")
code, out = oc("rollout", "latest", "dc/app")
check(until(lambda: rc_image(3) == ref(5), timeout=60), f"oc rollout latest resolves the trigger (processTriggers): {code} {out[:150]} {rc_image(3)}")

# 4. A BuildConfig.
st, out = req("POST", f"/apis/build.openshift.io/v1/namespaces/{NS}/buildconfigs", {
    "apiVersion": "build.openshift.io/v1", "kind": "BuildConfig", "metadata": {"name": "app"},
    "spec": {"source": {"git": {"uri": "https://git.example/app.git"}},
             "strategy": {"type": "Docker", "dockerStrategy": {"from": {"kind": "ImageStreamTag", "name": "mystream:latest"}}},
             "output": {"to": {"kind": "DockerImage", "name": f"{HOST}/img/app:latest"}},
             "triggers": [{"type": "ConfigChange"}, {"type": "ImageChange", "imageChange": {}},
                          {"type": "Generic", "generic": {"secret": "s3cret"}}, {"type": "GitHub", "github": {"secret": "gh"}}]}})
check(st == 201, f"a BuildConfig with ImageChange and webhook triggers is admitted: {st} {str(out)[:200]}")
build = lambda n: get(f"/apis/build.openshift.io/v1/namespaces/{NS}/builds/app-{n}") or {}
base = lambda n: build(n).get("spec", {}).get("strategy", {}).get("dockerStrategy", {}).get("from", {}).get("name")
check(until(lambda: base(1) == ref(5)), f"the first build's base image is the tag's digest: {base(1)}")
bc = lambda: get(f"/apis/build.openshift.io/v1/namespaces/{NS}/buildconfigs/app") or {}
ict = (bc().get("status", {}).get("imageChangeTriggers") or [{}])[0]
check(ict.get("lastTriggeredImageID") == ref(5), f"status.imageChangeTriggers records it: {ict}")
push(6)
check(until(lambda: base(2) == ref(6)), f"a push builds from the new digest: {base(2)}")
cause = (build(2).get("spec", {}).get("triggeredBy") or [{}])[0]
check(cause.get("message") == "Image change" and cause.get("imageChangeBuild", {}).get("imageID") == ref(6), f"cause Image change: {cause}")
hook = f"/apis/build.openshift.io/v1/namespaces/{NS}/buildconfigs/app/webhooks"
st, out = req("POST", f"{hook}/s3cret/generic", {"git": {"ref": "master", "commit": "c0ffee", "message": "hi"}})
name = out.get("metadata", {}).get("name") if isinstance(out, dict) else None
b = (get(f"/apis/build.openshift.io/v1/namespaces/{NS}/builds/{name}") or {}) if name else {}
check(st == 200 and b.get("spec", {}).get("triggeredBy", [{}])[0].get("message") == "Generic WebHook"
      and b.get("spec", {}).get("revision", {}).get("git", {}).get("commit") == "c0ffee",
      f"the generic webhook starts a build at the posted commit: {st} {str(out)[:200]}")
st, out = req("POST", f"{hook}/wrong/generic", {})
check(st == 401, f"a wrong secret: 401: {st} {str(out)[:150]}")
before = len((get(f"/apis/build.openshift.io/v1/namespaces/{NS}/builds") or {}).get("items", []))
st, out = req("POST", f"{hook}/gh/github", raw="{}", headers={"X-GitHub-Event": "ping"})
after = len((get(f"/apis/build.openshift.io/v1/namespaces/{NS}/builds") or {}).get("items", []))
check(st == 200 and before == after, f"a GitHub ping: 200, no build: {st} {before}->{after}")
code, out = oc("set", "triggers", "bc/app")
check(code == 0 and "image" in out.lower() and "webhook" in out.lower(), f"oc set triggers bc/app lists them: {out[:300]}")

sys.exit(failed)
PY
kill "$REG" 2>/dev/null
report
