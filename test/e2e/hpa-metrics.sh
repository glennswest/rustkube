#!/usr/bin/env bash
#
# metrics.k8s.io from cadvisor and the HPA on it (#89), on a real control
# plane on fastetcd. A stub cadvisor on 127.0.0.1 answers
# /api/v1.3/subcontainers/ the way cadvisor does for containerd pods: the
# root cgroup ("/") and one cgroup per container of every Pod in namespace
# hpa, labelled io.kubernetes.pod.{namespace,name} / container.name, using
# $W/cpu cores each (the rig sets it). Node n1's InternalIP is 127.0.0.1;
# there is no kubelet, so the rig marks Pods Running and Ready itself.
#
# - NodeMetrics for n1: the root cgroup's CPU rate and working set
# - PodMetrics for the Deployment's Pods, from their labelled cgroups
# - HPA web (cpu 50 % of a 100m request, min 1, max 5), pods at 200 %:
#   Deployment scaled up to 5 (maxReplicas: ScalingLimited TooManyReplicas),
#   currentMetrics averageUtilization 200, ScalingActive ValidMetricFound
# - usage to 0 with scaleDown stabilizationWindowSeconds 0: back to 1
# - with the stub stopped: ScalingActive=False FailedGetResourceMetric, and
#   the count is left alone
#
# Exit status is the number of failed checks.
mkdir -p "$PWD/tmp"
HT=$(mktemp -d "$PWD/tmp/hpa-metrics.XXXXXX")
# The stub's port, free now, handed to the apiserver lib.sh starts.
CADVISOR_PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
RK_APISERVER_ARGS="${RK_APISERVER_ARGS:-} --cadvisor-port $CADVISOR_PORT"
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
trap 'cleanup; rm -rf "$HT"' EXIT
start_controller_manager
start_scheduler
echo 0.2 >"$HT/cpu"

export API ADMIN HT
python3 - "$CADVISOR_PORT" <<'PY' >"$HT/stub.log" 2>&1 &
import http.client, http.server, json, os, ssl, sys, time, urllib.parse
port = int(sys.argv[1]); HT = os.environ["HT"]
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
start = time.time()
def pods():
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=10)
    c.request("GET", "/api/v1/namespaces/hpa/pods", headers={"Authorization": "Bearer " + os.environ["ADMIN"]})
    items = json.loads(c.getresponse().read()).get("items", []); c.close(); return items
def cg(name, labels, cores, mem):
    t1 = time.time() - start; t0 = t1 - 10
    ts = lambda t: time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(start + t)) + ".%06dZ" % int(((start + t) % 1) * 1e6)
    return {"name": name, "spec": {"labels": labels}, "stats": [
        {"timestamp": ts(t0), "cpu": {"usage": {"total": int(t0 * cores * 1e9)}}, "memory": {"working_set": mem}},
        {"timestamp": ts(t1), "cpu": {"usage": {"total": int(t1 * cores * 1e9)}}, "memory": {"working_set": mem}}]}
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        if self.path != "/api/v1.3/subcontainers/":
            self.send_response(404); self.end_headers(); return
        cores = float(open(f"{HT}/cpu").read())
        out = [cg("/", {}, 3.0, 8 << 30)]
        for p in pods():
            n = p["metadata"]["name"]
            labels = {"io.kubernetes.pod.namespace": "hpa", "io.kubernetes.pod.name": n}
            out.append(cg(f"/kubepods/{n}/sandbox", dict(labels, **{"io.kubernetes.container.name": "POD"}), 0.5, 1 << 20))
            out.append(cg(f"/kubepods/{n}/app", dict(labels, **{"io.kubernetes.container.name": "app"}), cores, 64 << 20))
        b = json.dumps(out).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
http.server.ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
PY
STUB=$!
echo "$STUB" >"$HT/stub.pid"

python3 - <<'PY' || FAIL=$?
import http.client, json, os, signal, ssl, sys, threading, time, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context(); HT = os.environ["HT"]
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, ctype="application/json"):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": ctype})
    r = c.getresponse(); raw = r.read(); c.close()
    try: return r.status, json.loads(raw) if raw else None
    except ValueError: return r.status, raw.decode(errors="replace")
def until(f, secs=60):
    end = time.time() + secs
    while time.time() < end:
        v = f()
        if v: return v
        time.sleep(1)
    return None

code, n = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node", "metadata": {"name": "n1", "labels": {"kubernetes.io/hostname": "n1"}}})
res = {"cpu": "64", "memory": "256Gi", "pods": "110"}
n["status"] = {"capacity": res, "allocatable": res, "addresses": [{"type": "InternalIP", "address": "127.0.0.1"}],
               "conditions": [{"type": "Ready", "status": "True"}]}
req("PUT", "/api/v1/nodes/n1/status", n)
req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "hpa"}})
req("POST", "/apis/apps/v1/namespaces/hpa/deployments", {"apiVersion": "apps/v1", "kind": "Deployment",
    "metadata": {"name": "web"}, "spec": {"replicas": 2, "selector": {"matchLabels": {"app": "web"}},
    "template": {"metadata": {"labels": {"app": "web"}}, "spec": {"containers": [
        {"name": "app", "image": "unused", "resources": {"requests": {"cpu": "100m", "memory": "64Mi"}}}]}}}})

# The kubelet's part: bound Pods go Running and Ready.
stop = threading.Event()
def kubelet():
    while not stop.is_set():
        _, l = req("GET", "/api/v1/namespaces/hpa/pods")
        for p in (l or {}).get("items", []):
            if p["spec"].get("nodeName") and p.get("status", {}).get("phase") != "Running":
                req("PATCH", f"/api/v1/namespaces/hpa/pods/{p['metadata']['name']}/status",
                    {"status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}},
                    "application/merge-patch+json")
        time.sleep(1)
threading.Thread(target=kubelet, daemon=True).start()
until(lambda: len([p for p in req("GET", "/api/v1/namespaces/hpa/pods")[1]["items"] if p["status"].get("phase") == "Running"]) == 2)

# --- metrics.k8s.io --------------------------------------------------------------
code, nm = req("GET", "/apis/metrics.k8s.io/v1beta1/nodes/n1")
check(code == 200 and nm.get("usage", {}).get("cpu", "").endswith("n") and abs(int(nm["usage"]["cpu"][:-1]) - 3e9) < 1e8,
      f"NodeMetrics n1: the root cgroup's 3 cores ({code} {nm.get('usage') if isinstance(nm, dict) else nm})")
code, pl = req("GET", "/apis/metrics.k8s.io/v1beta1/namespaces/hpa/pods?labelSelector=app%3Dweb")
ok = code == 200 and len(pl["items"]) == 2 and all(
    [c["name"] for c in m["containers"]] == ["app"] and abs(int(m["containers"][0]["usage"]["cpu"][:-1]) - 2e8) < 1e7
    for m in pl["items"])
check(ok, f"PodMetrics: two pods, container app at 200m, the sandbox left out ({code} {json.dumps(pl)[:300]})")
code, _ = req("GET", "/apis")
check(any(g["name"] == "metrics.k8s.io" for g in _["groups"]), "metrics.k8s.io is in discovery")

# --- scale up -----------------------------------------------------------------
req("POST", "/apis/autoscaling/v2/namespaces/hpa/horizontalpodautoscalers", {"apiVersion": "autoscaling/v2",
    "kind": "HorizontalPodAutoscaler", "metadata": {"name": "web"}, "spec": {
    "scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "web"}, "minReplicas": 1, "maxReplicas": 5,
    "metrics": [{"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": 50}}}],
    "behavior": {"scaleDown": {"stabilizationWindowSeconds": 0}}}})
d = until(lambda: (lambda d: d if d["spec"]["replicas"] == 5 else None)(req("GET", "/apis/apps/v1/namespaces/hpa/deployments/web")[1]), 90)
check(d is not None, "200 % of a 50 % target: the Deployment is scaled to maxReplicas 5")
_, h = req("GET", "/apis/autoscaling/v2/namespaces/hpa/horizontalpodautoscalers/web")
conds = {c["type"]: c for c in h.get("status", {}).get("conditions", [])}
cur = (h.get("status", {}).get("currentMetrics") or [{}])[0].get("resource", {}).get("current", {})
check(cur.get("averageUtilization") == 200, f"currentMetrics averageUtilization 200 ({cur})")
check(conds.get("ScalingActive", {}).get("reason") == "ValidMetricFound", f"ScalingActive ValidMetricFound ({conds.get('ScalingActive')})")
check(conds.get("ScalingLimited", {}).get("reason") in ("TooManyReplicas", "ScaleUpLimit"), f"ScalingLimited ({conds.get('ScalingLimited')})")

# --- scale down ---------------------------------------------------------------
open(f"{HT}/cpu", "w").write("0.0")
d = until(lambda: (lambda d: d if d["spec"]["replicas"] == 1 else None)(req("GET", "/apis/apps/v1/namespaces/hpa/deployments/web")[1]), 120)
check(d is not None, "idle pods, no stabilisation window: scaled down to minReplicas 1")

# --- no metrics ----------------------------------------------------------------
os.kill(int(open(f"{HT}/stub.pid").read()), signal.SIGTERM)
open(f"{HT}/cpu", "w").write("0.2")
def no_metrics():
    _, h = req("GET", "/apis/autoscaling/v2/namespaces/hpa/horizontalpodautoscalers/web")
    c = {c["type"]: c for c in h.get("status", {}).get("conditions", [])}
    return c if c.get("ScalingActive", {}).get("reason") == "FailedGetResourceMetric" else None
c = until(no_metrics, 60)
check(c is not None, f"cadvisor gone: ScalingActive=False FailedGetResourceMetric ({c and c['ScalingActive']['message'][:150]})")
_, d = req("GET", "/apis/apps/v1/namespaces/hpa/deployments/web")
check(d["spec"]["replicas"] == 1, f"…and the count is left alone ({d['spec']['replicas']})")
stop.set()
sys.exit(failed)
PY
report
