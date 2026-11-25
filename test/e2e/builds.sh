#!/usr/bin/env bash
#
# build.openshift.io Builds and BuildConfigs (#374, sc#151, sc#162) against a
# real apiserver, controller-manager (as itself, so its role and the
# privileged SCC are what admit build pods) and scheduler, with `oc`. The
# script plays the kubelet: one node; a build pod finishes Succeeded, or
# Failed with the builder's termination message when its BUILD's post-commit
# script is `exit 1`; the `slow` BuildConfig's build pods stay Running.
# Nothing here runs a real builder image: that a build clones, builds and
# pushes is the builder image's, proven on a node (stormcos).
#
#   1. a Docker BuildConfig with a ConfigChange trigger starts web-1; its pod
#      web-1-build is OpenShift's (builder image, git-clone and
#      manage-dockerfile, docker-build privileged, BUILD carrying the build);
#      web-1 Completes; `oc get bc`, `oc get builds` print OpenShift's columns
#   2. `oc start-build` → web-2; `oc cancel-build` stops a slow build
#      (Cancelled, its pod deleted)
#   3. `oc set build-hook --post-commit --script='exit 1'` reaches the next
#      build's BUILD, and that build Fails with the log snippet
#   4. `oc set build-secret --source/--push/--pull` mount at OpenShift's paths
#   5. `oc adm prune builds --keep-complete=0 --keep-failed=0 --confirm`
#   6. a user may run only the strategies bound to them (BuildByStrategy):
#      without system:build-strategy-docker, 403
#   7. JenkinsPipeline and Binary sources are refused (422), a GitHub trigger
#      without a secret too (webhooks are served since #386, rig image-triggers);
#      the namespace has `builder` and `system:image-builders`
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
RK_CM_ARGS="--build-image registry.local/origin-docker-builder:5.0.0 --build-insecure-registries 10.0.0.5:5100"
start_controller_manager
start_scheduler
if ! command -v oc >/dev/null; then
  if [ -x "${RK_TOOLS:-}/oc" ]; then
    OC=$RK_TOOLS/oc
  else
    curl -sfL https://mirror.openshift.com/pub/openshift-v4/x86_64/clients/ocp/stable/openshift-client-linux.tar.gz \
      | tar -xz -C "$W" oc || exit 100
    OC=$W/oc
  fi
else
  OC=$(command -v oc)
fi
ALICE=$(token alice '[]')
export API ADMIN ALICE OC W
python3 - <<'PY' || FAIL=$?
import datetime, http.client, json, os, ssl, subprocess, sys, threading, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"]); TOKEN = os.environ["ADMIN"]; OC = os.environ["OC"]; W = os.environ["W"]
CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(method, path, body=None, token=None, ctype="application/json"):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + (token or TOKEN), "Content-Type": ctype})
    r = conn.getresponse(); data = r.read()
    try:
        return r.status, json.loads(data)
    except ValueError:
        return r.status, data.decode(errors="replace")

def get(path):
    st, v = req("GET", path)
    return v if st == 200 else None

def oc(*args, token=None, timeout=120):
    p = subprocess.run([OC, "--server", os.environ["API"], "--insecure-skip-tls-verify", "--token", token or TOKEN,
                        "--cache-dir", f"{W}/oc-cache", "-n", NS, *args],
                       capture_output=True, text=True, timeout=timeout, env={**os.environ, "KUBECONFIG": f"{W}/oc-kc"})
    return p.returncode, p.stdout + p.stderr

def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")

NS, NODE = "bld", "n1"
req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": NS}})
req("POST", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases", {"apiVersion": "coordination.k8s.io/v1",
    "kind": "Lease", "metadata": {"name": NODE}, "spec": {"holderIdentity": NODE, "leaseDurationSeconds": 40, "renewTime": now()}})
_, n = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node",
    "metadata": {"name": NODE, "labels": {"kubernetes.io/hostname": NODE}}})
res = {"cpu": "64", "memory": "256Gi", "pods": "110"}
n["status"] = {"capacity": res, "allocatable": res, "conditions": [{"type": "Ready", "status": "True"}]}
req("PUT", f"/api/v1/nodes/{NODE}/status", n)
for s in ("gitcreds", "pushcreds", "pullcreds"):
    req("POST", f"/api/v1/namespaces/{NS}/secrets", {"apiVersion": "v1", "kind": "Secret", "metadata": {"name": s},
        "type": "Opaque", "stringData": {"x": "y"}})

stop = threading.Event()
def kubelet():
    """The stand-in kubelet: a build pod finishes as the builder would."""
    last = 0
    while not stop.is_set():
        try:
            if time.monotonic() - last > 5:
                l = get(f"/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/{NODE}")
                l["spec"]["renewTime"] = now()
                req("PUT", f"/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/{NODE}", l)
                last = time.monotonic()
            for p in get(f"/api/v1/namespaces/{NS}/pods")["items"]:
                name = p["metadata"]["name"]
                if p["metadata"].get("deletionTimestamp"):
                    req("DELETE", f"/api/v1/namespaces/{NS}/pods/{name}?gracePeriodSeconds=0")
                    continue
                if not p["spec"].get("nodeName") or p.get("status", {}).get("phase") in ("Succeeded", "Failed"):
                    continue
                main = p["spec"]["containers"][0]
                build = json.loads(next((e["value"] for e in main.get("env", []) if e["name"] == "BUILD"), "{}"))
                inits = [{"name": c["name"], "state": {"terminated": {"exitCode": 0}}} for c in p["spec"].get("initContainers", [])]
                if build.get("metadata", {}).get("annotations", {}).get("openshift.io/build-config.name") == "slow":
                    st = {"phase": "Running", "startTime": now(), "initContainerStatuses": inits,
                          "containerStatuses": [{"name": main["name"], "state": {"running": {}}}]}
                elif build.get("spec", {}).get("postCommit", {}).get("script") == "exit 1":
                    st = {"phase": "Failed", "startTime": now(), "initContainerStatuses": inits,
                          "containerStatuses": [{"name": main["name"], "state": {"terminated": {"exitCode": 1,
                              "message": "STEP 2/2: RUN true\nerror: build error: running 'exit 1' failed with exit code 1\n"}}}]}
                else:
                    st = {"phase": "Succeeded", "startTime": now(), "initContainerStatuses": inits,
                          "containerStatuses": [{"name": main["name"], "state": {"terminated": {"exitCode": 0}}}]}
                if p.get("status", {}).get("phase") == st["phase"]:
                    continue
                p["status"] = st
                req("PUT", f"/api/v1/namespaces/{NS}/pods/{name}/status", p)
        except Exception as e:
            print("kubelet:", e, flush=True)
        time.sleep(0.3)
threading.Thread(target=kubelet, daemon=True).start()

def until(f, limit=90):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        v = f()
        if v: return v
        time.sleep(0.5)
    return f()

def build(name):
    return get(f"/apis/build.openshift.io/v1/namespaces/{NS}/builds/{name}")
def phase(name):
    b = build(name)
    return b and b["status"].get("phase")
def pod(name):
    return get(f"/api/v1/namespaces/{NS}/pods/{name}-build")

def bc(name, extra=None, token=None):
    spec = {"triggers": [{"type": "ConfigChange"}], "runPolicy": "Serial",
            "source": {"git": {"uri": "https://git.example/web.git"}},
            "strategy": {"dockerStrategy": {}},
            "output": {"to": {"kind": "DockerImage", "name": "10.0.0.5:5100/bld/web:latest"}}}
    spec.update(extra or {})
    return req("POST", f"/apis/build.openshift.io/v1/namespaces/{NS}/buildconfigs",
               {"apiVersion": "build.openshift.io/v1", "kind": "BuildConfig", "metadata": {"name": name}, "spec": spec}, token)

# 7 (setup half): the builder account and its binding.
check(until(lambda: get(f"/api/v1/namespaces/{NS}/serviceaccounts/builder") is not None), "the namespace has a builder ServiceAccount")
rb = until(lambda: get(f"/apis/rbac.authorization.k8s.io/v1/namespaces/{NS}/rolebindings/system:image-builders"))
check(rb is not None and rb["roleRef"]["name"] == "system:image-builder"
      and rb["subjects"][0]["name"] == "builder", f"and system:image-builders binds it: {rb and rb['roleRef']}")

# 1.
st, out = bc("web")
check(st == 201, f"a Docker BuildConfig is created ({st} {str(out)[:200]})")
check(until(lambda: phase("web-1") == "Complete"), f"its ConfigChange trigger starts web-1, which completes: {phase('web-1')}")
p = until(lambda: pod("web-1"))
inits = [c["name"] for c in (p or {}).get("spec", {}).get("initContainers", [])]
main = (p or {}).get("spec", {}).get("containers", [{}])[0]
check(inits == ["git-clone", "manage-dockerfile"] and main.get("name") == "docker-build"
      and main.get("image") == "registry.local/origin-docker-builder:5.0.0" and main.get("args") == ["openshift-docker-build"]
      and main.get("securityContext", {}).get("privileged") is True, f"web-1-build is OpenShift's build pod: {inits} {main.get('name')}")
check(p is not None and p["metadata"]["annotations"].get("openshift.io/scc") == "privileged",
      f"admitted under the privileged SCC: {p and p['metadata']['annotations'].get('openshift.io/scc')}")
cm = get(f"/api/v1/namespaces/{NS}/configmaps/web-1-sys-config")
check(cm is not None and 'location = "10.0.0.5:5100"' in cm.get("data", {}).get("registries.conf", ""),
      "its registries.conf lets it reach the node registry over HTTP")
b1 = build("web-1")
check(b1["status"].get("completionTimestamp") and b1["status"].get("startTimestamp") and b1["status"].get("duration") is not None
      and b1["metadata"]["annotations"].get("openshift.io/build.pod-name") == "web-1-build", f"completion data: {b1['status']}")
code, out = oc("get", "bc")
check(code == 0 and "LATEST" in out and "Docker" in out and "Git" in out, f"oc get bc: {out.strip()}")
code, out = oc("get", "builds")
check(code == 0 and "web-1" in out and "Complete" in out and "DURATION" in out, f"oc get builds: {out.strip()}")

# 2.
code, out = oc("start-build", "web")
check(code == 0 and "web-2" in out, f"oc start-build: {out.strip()}")
check(until(lambda: phase("web-2") == "Complete"), "web-2 completes")
bc("slow")
check(until(lambda: phase("slow-1") == "Running"), "a slow build runs")
code, out = oc("cancel-build", "slow-1")
check(code == 0, f"oc cancel-build: {out.strip()}")
check(until(lambda: phase("slow-1") == "Cancelled"), "it is Cancelled")
check(until(lambda: pod("slow-1") is None), "and its pod is deleted")

# 3.
code, out = oc("set", "build-hook", "bc/web", "--post-commit", "--script=exit 1")
check(code == 0, f"oc set build-hook --post-commit: {out.strip()}")
code, out = oc("start-build", "web")
check(until(lambda: phase("web-3") == "Failed"), f"web-3 fails on its post-commit hook: {phase('web-3')}")
p3 = pod("web-3")
b3j = json.loads(next(e["value"] for e in p3["spec"]["containers"][0]["env"] if e["name"] == "BUILD"))
check(b3j["spec"]["postCommit"] == {"script": "exit 1"}, f"the hook reached the builder: {b3j['spec'].get('postCommit')}")
b3 = until(lambda: (build("web-3") or {}).get("status", {}).get("logSnippet") and build("web-3"))
check(b3 and "running 'exit 1' failed" in b3["status"].get("logSnippet", "")
      and b3["status"].get("reason") == "GenericBuildFailed", f"its log snippet and reason: {b3 and b3['status']}")
oc("set", "build-hook", "bc/web", "--post-commit", "--remove")

# 4.
for kind, sec in (("--source", "gitcreds"), ("--push", "pushcreds"), ("--pull", "pullcreds")):
    code, out = oc("set", "build-secret", kind, "bc/web", sec)
    check(code == 0, f"oc set build-secret {kind}: {out.strip()}")
code, out = oc("start-build", "web")
check(until(lambda: phase("web-4") == "Complete"), "web-4 completes")
p4 = pod("web-4")
mounts = lambda c: {m["mountPath"]: m["name"] for m in c.get("volumeMounts", [])}
git = next(c for c in p4["spec"]["initContainers"] if c["name"] == "git-clone")
vols = {v["name"]: v for v in p4["spec"]["volumes"]}
srcvol = mounts(git).get("/var/run/secrets/openshift.io/source")
check(srcvol and vols[srcvol]["secret"]["secretName"] == "gitcreds", "the source secret is on git-clone, at OpenShift's path")
m = mounts(p4["spec"]["containers"][0])
check(vols.get(m.get("/var/run/secrets/openshift.io/push"), {}).get("secret", {}).get("secretName") == "pushcreds"
      and vols.get(m.get("/var/run/secrets/openshift.io/pull"), {}).get("secret", {}).get("secretName") == "pullcreds",
      "the push and pull secrets are on the build, at OpenShift's paths")

# 5.
code, out = oc("adm", "prune", "builds", "--keep-complete=0", "--keep-failed=0", "--keep-younger-than=0s", "--confirm")
check(code == 0, f"oc adm prune builds: {out.strip()[:400]}")
check(until(lambda: all(build(f"web-{i}") is None for i in (1, 2, 3, 4)) and build("slow-1") is None),
      "the finished builds are pruned")

# 6.
req("POST", f"/apis/rbac.authorization.k8s.io/v1/namespaces/{NS}/rolebindings", {"apiVersion": "rbac.authorization.k8s.io/v1",
    "kind": "RoleBinding", "metadata": {"name": "alice-edit"},
    "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "edit"},
    "subjects": [{"apiGroup": "rbac.authorization.k8s.io", "kind": "User", "name": "alice"}]})
st, _ = req("DELETE", "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings/system:build-strategy-docker-binding")
st, out = bc("alice", token=os.environ["ALICE"])
check(st == 403 and "build strategy Docker is not allowed" in json.dumps(out), f"without builds/docker a Docker BuildConfig is refused: {st} {str(out)[:200]}")
req("POST", "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings", {"apiVersion": "rbac.authorization.k8s.io/v1",
    "kind": "ClusterRoleBinding", "metadata": {"name": "system:build-strategy-docker-binding"},
    "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "system:build-strategy-docker"},
    "subjects": [{"apiGroup": "rbac.authorization.k8s.io", "kind": "Group", "name": "system:authenticated"}]})
st, out = bc("alice", token=os.environ["ALICE"])
check(st == 201, f"with it bound again, alice creates it: {st}")

# 7.
for what, extra, want in (
        ("JenkinsPipeline", {"strategy": {"jenkinsPipelineStrategy": {"jenkinsfile": "node {}"}}}, "Unsupported value: \\\"JenkinsPipeline\\\""),
        ("a Binary source", {"source": {"binary": {}}}, "Unsupported value: \\\"Binary\\\""),
        ("a GitHub trigger without a secret", {"triggers": [{"type": "GitHub", "github": {}}]}, "spec.triggers[0].github.secret: Required value")):
    st, out = bc("refused", extra)
    check(st == 422 and want in json.dumps(out), f"{what} is refused: {st} {str(out)[:200]}")

stop.set()
sys.exit(failed)
PY
report
