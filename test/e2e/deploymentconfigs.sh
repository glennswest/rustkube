#!/usr/bin/env bash
#
# apps.openshift.io DeploymentConfigs (#375, sc#151, sc#162) against a real
# apiserver, controller-manager and scheduler, with `oc`. The script plays
# the kubelet: one node; bound pods Running and Ready; a hook pod finishes
# Succeeded, or Failed when its command is /bin/false.
#
#   1. a Recreate DC with a pre hook rolls out web-1: the hook pod runs, the
#      RC reaches 2 available, the DC reports it; `oc get dc` prints it;
#      `oc logs dc/web` shows the deployer's log
#   2. `oc rollout latest` → web-2, web-1 scaled to 0; a template change
#      (`oc set env`) → web-3 by the ConfigChange trigger
#   3. `oc set deployment-hook --pre --failure-policy=abort -- /bin/false`:
#      the next rollout fails, the DC is back on web-3; `--remove`, then it
#      rolls out
#   4. `oc rollout undo` rolls back to the previous template; `oc scale`
#   5. a Rolling DC rolls 3 replicas from one version to the next, its log
#      the rolling updater's
#   6. `oc rollout cancel` stops a rollout whose pods never become ready
#   7. `oc adm prune deployments --keep-complete=1 --confirm` deletes the
#      oldest complete versions
#   8. a Custom strategy (#392): `custom-1-deploy` runs the strategy's image
#      as the namespace's `deployer` account. The script plays that image,
#      scaling with a token minted for `deployer`, so `system:deployers`
#      is what lets it. Its version completes; `oc rollout latest` moves to
#      custom-2 and the image scales custom-1 down; the deployer account
#      cannot read secrets; `oc logs dc/custom` serves the version's log;
#      an image that fails fails its version and the DC stays on custom-2.
#      An ImageChange trigger and a Custom strategy without an image are
#      refused (422)
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
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
export API ADMIN OC W
python3 - <<'PY' || FAIL=$?
import datetime, http.client, json, os, ssl, subprocess, sys, threading, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"]); TOKEN = os.environ["ADMIN"]; OC = os.environ["OC"]; W = os.environ["W"]
CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1

def req(method, path, body=None, ctype="application/json", ok=(200, 201, 404, 409, 422), token=None):
    conn = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    conn.request(method, path, body=None if body is None else json.dumps(body),
                 headers={"Authorization": "Bearer " + (token or TOKEN), "Content-Type": ctype})
    r = conn.getresponse(); data = r.read()
    if r.status not in ok:
        print(f"setup {method} {path}: {r.status} {data[:300]!r}"); sys.exit(100)
    try:
        return r.status, json.loads(data)
    except ValueError:
        return r.status, data.decode(errors="replace")

def get(path):
    st, v = req("GET", path)
    return v if st == 200 else None

def oc(*args, timeout=120):
    p = subprocess.run([OC, "--server", os.environ["API"], "--insecure-skip-tls-verify", "--token", TOKEN,
                        "--cache-dir", f"{W}/oc-cache", "-n", NS, *args],
                       capture_output=True, text=True, timeout=timeout, env={**os.environ, "KUBECONFIG": f"{W}/oc-kc"})
    return p.returncode, p.stdout + p.stderr

def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")

NS, NODE = "dcs", "n1"
req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": NS}})
req("POST", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases", {"apiVersion": "coordination.k8s.io/v1",
    "kind": "Lease", "metadata": {"name": NODE}, "spec": {"holderIdentity": NODE, "leaseDurationSeconds": 40, "renewTime": now()}})
_, n = req("POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node",
    "metadata": {"name": NODE, "labels": {"kubernetes.io/hostname": NODE}}})
res = {"cpu": "64", "memory": "256Gi", "pods": "110"}
n["status"] = {"capacity": res, "allocatable": res, "conditions": [{"type": "Ready", "status": "True"}]}
req("PUT", f"/api/v1/nodes/{NODE}/status", n)

stop = threading.Event()
deployer_calls = []  # (what, status) of the stand-in Custom image's API calls

def custom_image(p):
    """A Custom strategy's image, as a real one does it: the version up to
    its desired replicas, the config's other versions down to 0, through
    the API as the pod's own account; MODE=fail exits 1 instead."""
    name = p["metadata"]["name"]
    env = {e["name"]: e.get("value") for e in p["spec"]["containers"][0].get("env", [])}
    ok = True
    try:
        sa = p["spec"]["serviceAccountName"]
        _, tr = req("POST", f"/api/v1/namespaces/{NS}/serviceaccounts/{sa}/token",
                    {"apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest", "spec": {}})
        tok = tr["status"]["token"]
        ok_codes = (200, 201, 403, 404, 409, 422)
        if env.get("MODE") == "fail":
            ok = False
        else:
            def scale(rcname, replicas):
                # GET then PUT, again on a conflict, as client-go's retry does.
                for _ in range(10):
                    path = f"/api/v1/namespaces/{NS}/replicationcontrollers/{rcname}"
                    st, r = req("GET", path, ok=ok_codes, token=tok)
                    if st != 200:
                        return st
                    r["spec"]["replicas"] = replicas
                    st, _ = req("PUT", path, r, ok=ok_codes, token=tok)
                    if st != 409:
                        return st
                    time.sleep(0.2)
                return st
            new = env["OPENSHIFT_DEPLOYMENT_NAME"]
            st, r = req("GET", f"/api/v1/namespaces/{NS}/replicationcontrollers/{new}", ok=ok_codes, token=tok)
            deployer_calls.append(("get " + new, st))
            want = int(r["metadata"]["annotations"]["kubectl.kubernetes.io/desired-replicas"])
            deployer_calls.append(("scale up " + new, scale(new, want)))
            until(lambda: (rc(new) or {}).get("status", {}).get("availableReplicas") == want, 60)
            config = r["metadata"]["annotations"]["openshift.io/deployment-config.name"]
            st, l = req("GET", f"/api/v1/namespaces/{NS}/replicationcontrollers?labelSelector="
                        f"openshift.io%2Fdeployment-config.name%3D{config}", ok=ok_codes, token=tok)
            deployer_calls.append(("list versions", st))
            for o in l.get("items", []) if st == 200 else []:
                if o["metadata"]["name"] != new and o["spec"].get("replicas"):
                    deployer_calls.append(("scale down " + o["metadata"]["name"], scale(o["metadata"]["name"], 0)))
            st, _ = req("GET", f"/api/v1/namespaces/{NS}/secrets", ok=ok_codes, token=tok)
            deployer_calls.append(("secrets", st))
    except Exception as e:
        print("custom image:", e, flush=True)
        ok = False
    p = get(f"/api/v1/namespaces/{NS}/pods/{name}")
    if p:
        p["status"] = {"phase": "Succeeded" if ok else "Failed", "message": "" if ok else "exit code 1",
                       "containerStatuses": [{"name": "deployment", "restartCount": 0, "state":
                           {"terminated": {"exitCode": 0 if ok else 1, "reason": "Completed" if ok else "Error"}}}]}
        req("PUT", f"/api/v1/namespaces/{NS}/pods/{name}/status", p)

def kubelet():
    """The stand-in kubelet: Lease renewed; bound pods Running and Ready,
    but a pod labelled stall=yes never Ready; hook pods finish by their
    command; terminating pods gone."""
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
                if not p["spec"].get("nodeName") or p.get("status", {}).get("phase") in ("Running", "Succeeded", "Failed"):
                    continue
                labels = p["metadata"].get("labels", {})
                if labels.get("openshift.io/deployer-pod-for.name") and not labels.get("openshift.io/deployer-pod.type"):
                    p["status"] = {"phase": "Running", "conditions": [{"type": "PodScheduled", "status": "True"}]}
                    req("PUT", f"/api/v1/namespaces/{NS}/pods/{name}/status", p)
                    threading.Thread(target=custom_image, args=(get(f"/api/v1/namespaces/{NS}/pods/{name}"),),
                                     daemon=True).start()
                    continue
                if labels.get("openshift.io/deployer-pod.type"):
                    bad = p["spec"]["containers"][0].get("command") == ["/bin/false"]
                    p["status"] = {"phase": "Failed" if bad else "Succeeded", "message": "exit code 1" if bad else "",
                                   "containerStatuses": [{"name": "lifecycle", "restartCount": 0, "state":
                                       {"terminated": {"exitCode": 1 if bad else 0, "reason": "Error" if bad else "Completed"}}}]}
                else:
                    ready = "False" if p["metadata"].get("labels", {}).get("stall") == "yes" else "True"
                    p["status"] = {"phase": "Running", "conditions": [
                        {"type": "PodScheduled", "status": "True"}, {"type": "Ready", "status": ready}]}
                req("PUT", f"/api/v1/namespaces/{NS}/pods/{name}/status", p)
        except Exception as e:  # keep playing the kubelet
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

def dc(name):
    return get(f"/apis/apps.openshift.io/v1/namespaces/{NS}/deploymentconfigs/{name}")
def rc(name):
    return get(f"/api/v1/namespaces/{NS}/replicationcontrollers/{name}")
def phase(name):
    r = rc(name)
    return r and r["metadata"].get("annotations", {}).get("openshift.io/deployment.phase")
def settled(name, version, want):
    d = dc(name)
    return d and d["status"].get("latestVersion") == version and phase(f"{name}-{version}") == "Complete" \
        and d["status"].get("availableReplicas") == want and d["status"].get("updatedReplicas") == want

def make(name, strategy, replicas, extra_labels=None):
    labels = {"app": name, **(extra_labels or {})}
    st, out = req("POST", f"/apis/apps.openshift.io/v1/namespaces/{NS}/deploymentconfigs", {
        "apiVersion": "apps.openshift.io/v1", "kind": "DeploymentConfig", "metadata": {"name": name},
        "spec": {"replicas": replicas, "selector": {"app": name}, "strategy": strategy, "revisionHistoryLimit": 10,
                 "template": {"metadata": {"labels": labels},
                              "spec": {"containers": [{"name": "web", "image": "registry.local/web:1"}]}}}})
    return st, out

# 1.
st, _ = make("web", {"type": "Recreate", "recreateParams": {"pre": {"failurePolicy": "Abort",
              "execNewPod": {"command": ["/bin/true"]}}}}, 2)
check(st == 201, f"a Recreate DeploymentConfig with a pre hook is created ({st})")
check(until(lambda: settled("web", 1, 2)), f"web-1 rolls out to 2 available: {dc('web')['status']}")
hook = get(f"/api/v1/namespaces/{NS}/pods/web-1-hook-pre")
check(hook is not None and hook["spec"]["containers"][0]["image"] == "registry.local/web:1"
      and hook["metadata"]["labels"].get("openshift.io/deployer-pod-for.name") == "web-1",
      "the pre hook ran as web-1-hook-pre, from the container's image")
code, out = oc("get", "dc", "web")
check(code == 0 and "REVISION" in out and "TRIGGERED BY" in out and "config" in out, f"oc get dc: {out.strip()}")
code, out = oc("logs", "dc/web")
check(code == 0 and "--> pre: Running hook pod ..." in out and "--> pre: Success" in out and "--> Success" in out,
      f"oc logs dc/web is the deployer's log: {out.strip()!r}")
conds = {c["type"]: c for c in dc("web")["status"].get("conditions", [])}
check(conds.get("Available", {}).get("status") == "True"
      and conds.get("Progressing", {}).get("reason") == "NewReplicationControllerAvailable", f"conditions: {conds}")

# 2.
code, out = oc("rollout", "latest", "dc/web")
check(code == 0, f"oc rollout latest: {out.strip()}")
check(until(lambda: settled("web", 2, 2)), "web-2 rolls out")
check(until(lambda: (rc("web-1") or {}).get("spec", {}).get("replicas") == 0), "web-1 is scaled to 0")
code, out = oc("set", "env", "dc/web", "COLOR=blue")
check(code == 0, f"oc set env: {out.strip()}")
check(until(lambda: settled("web", 3, 2)), "a template change rolls out web-3 (ConfigChange)")
check(dc("web")["status"].get("details", {}).get("causes", [{}])[0].get("type") == "ConfigChange", "its cause is ConfigChange")

# 3.
code, out = oc("set", "deployment-hook", "dc/web", "--pre", "--failure-policy=abort", "-c", "web", "--", "/bin/false")
check(code == 0, f"oc set deployment-hook --pre: {out.strip()}")
# A strategy change is no template change: no ConfigChange rollout of its own.
code, out = oc("rollout", "latest", "dc/web")
check(code == 0, f"oc rollout latest: {out.strip()}")
check(until(lambda: phase("web-4") == "Failed"), "the rollout of web-4 fails on its pre hook")
check(until(lambda: (rc("web-3") or {}).get("spec", {}).get("replicas") == 2 and (rc("web-4") or {}).get("spec", {}).get("replicas") == 0),
      "the DC stays on web-3, web-4 at 0")
code, out = oc("logs", "dc/web")
check("pre hook failed" in out and "--> pre: Failed" in out, f"its log says why: {out.strip()!r}")
code, out = oc("set", "deployment-hook", "dc/web", "--pre", "--remove")
check(code == 0, f"oc set deployment-hook --remove: {out.strip()}")
code, out = oc("rollout", "latest", "dc/web")
check(until(lambda: settled("web", 5, 2)), f"without the hook web-5 rolls out ({out.strip()})")

# 4.
code, out = oc("set", "env", "dc/web", "COLOR=green")
check(until(lambda: settled("web", 6, 2)), "COLOR=green rolls out web-6")
code, out = oc("rollout", "undo", "dc/web")
check(code == 0, f"oc rollout undo: {out.strip()}")
check(until(lambda: settled("web", 7, 2)), "the undo rolls out web-7")
env = dc("web")["spec"]["template"]["spec"]["containers"][0].get("env", [])
check({"name": "COLOR", "value": "blue"} in env, f"with web-5's template: {env}")
code, out = oc("scale", "dc/web", "--replicas=3")
check(code == 0 and until(lambda: dc("web")["status"].get("availableReplicas") == 3
                          and dc("web")["spec"]["replicas"] == 3), f"oc scale dc/web --replicas=3: {out.strip()}")

# 5.
st, _ = make("roll", {"type": "Rolling", "rollingParams": {"maxSurge": 1, "maxUnavailable": 0}}, 3)
check(until(lambda: settled("roll", 1, 3)), "a Rolling DC rolls out roll-1 to 3")
code, out = oc("set", "env", "dc/roll", "V=2")
check(until(lambda: settled("roll", 2, 3)) and (rc("roll-1") or {})["spec"]["replicas"] == 0,
      "roll-2 replaces roll-1, 3 available")
code, out = oc("logs", "dc/roll")
check("--> Scaling up roll-2 from 0 to 3, scaling down roll-1 from 3 to 0" in out and "    Scaling roll-2 up to" in out,
      f"the rolling updater's log: {out.strip()!r}")

# 6.
st, _ = make("stuck", {"type": "Recreate"}, 1, {"stall": "yes"})
check(until(lambda: phase("stuck-1") == "Running"), "a rollout whose pod never becomes ready runs")
code, out = oc("rollout", "cancel", "dc/stuck")
check(code == 0, f"oc rollout cancel: {out.strip()}")
check(until(lambda: phase("stuck-1") == "Failed"), "it is Failed")
conds = {c["type"]: c for c in dc("stuck")["status"].get("conditions", [])}
check(until(lambda: {c["type"]: c for c in dc("stuck")["status"].get("conditions", [])}.get("Progressing", {}).get("reason") == "RolloutCancelled"),
      "the DC says RolloutCancelled")

# 7.
code, out = oc("adm", "prune", "deployments", "--keep-complete=1", "--keep-failed=1", "--keep-younger-than=0s", "--confirm")
check(code == 0, f"oc adm prune deployments: {out.strip()[:400]}")
gone = [f"web-{v}" for v in (1, 2) if rc(f"web-{v}") is None]
check(gone == ["web-1", "web-2"] and rc("web-7") is not None, f"the oldest complete versions are pruned: {gone}")

# 8.
sa = until(lambda: get(f"/api/v1/namespaces/{NS}/serviceaccounts/deployer"), 30)
rb = until(lambda: get(f"/apis/rbac.authorization.k8s.io/v1/namespaces/{NS}/rolebindings/system:deployers"), 30)
check(sa is not None and rb is not None and rb["roleRef"]["name"] == "system:deployer"
      and rb["subjects"][0]["name"] == "deployer", f"the deployer account and system:deployers: {rb and rb['roleRef']}")
custom = {"type": "Custom", "customParams": {"image": "registry.local/deployer:1", "command": ["/deploy"],
                                             "environment": [{"name": "MODE", "value": "ok"}]}}
st, out = make("custom", custom, 2)
check(st == 201, f"a Custom DC is admitted: {st} {str(out)[:200]}")
check(until(lambda: settled("custom", 1, 2)), f"custom-1 completes with 2 available: {phase('custom-1')}")
pod = get(f"/api/v1/namespaces/{NS}/pods/custom-1-deploy")
c = pod and pod["spec"]["containers"][0]
env = {e["name"]: e.get("value") for e in (c or {}).get("env", [])}
check(pod is not None and pod["spec"]["serviceAccountName"] == "deployer" and c["image"] == "registry.local/deployer:1"
      and c["command"] == ["/deploy"] and env.get("MODE") == "ok" and env.get("OPENSHIFT_DEPLOYMENT_NAME") == "custom-1"
      and env.get("OPENSHIFT_DEPLOYMENT_NAMESPACE") == NS and pod["status"].get("phase") == "Succeeded"
      and pod["metadata"]["ownerReferences"][0]["name"] == "custom-1",
      f"custom-1-deploy ran the strategy's image as deployer: {pod and pod['spec'].get('serviceAccountName')} {c and c.get('image')}")
code, out = oc("rollout", "latest", "dc/custom")
check(code == 0, f"oc rollout latest dc/custom: {out.strip()[:200]}")
check(until(lambda: settled("custom", 2, 2) and (rc("custom-1") or {}).get("spec", {}).get("replicas") == 0),
      f"custom-2 completes and the image scaled custom-1 down: {phase('custom-2')}")
calls = dict(deployer_calls)
check(calls.get("scale up custom-2") == 200 and calls.get("scale down custom-1") == 200 and calls.get("list versions") == 200,
      f"the deployer account scaled the versions: {deployer_calls}")
check(calls.get("secrets") == 403, f"and may not read secrets: {calls.get('secrets')}")
code, out = oc("logs", "dc/custom")
check(code == 0 and "Running deployer pod custom-2-deploy" in out, f"oc logs dc/custom: {out.strip()[:300]}")
d = dc("custom")
d["spec"]["strategy"]["customParams"]["environment"] = [{"name": "MODE", "value": "fail"}]
req("PUT", f"/apis/apps.openshift.io/v1/namespaces/{NS}/deploymentconfigs/custom", d)
code, out = oc("rollout", "latest", "dc/custom")
check(until(lambda: phase("custom-3") == "Failed"), f"an image that fails fails custom-3: {phase('custom-3')}")
r3 = rc("custom-3")
check(r3 and "exit code 1" in r3["metadata"]["annotations"].get("openshift.io/deployment.status-reason", ""),
      f"with the pod's reason: {r3 and r3['metadata']['annotations'].get('openshift.io/deployment.status-reason')}")
check(until(lambda: (rc("custom-2") or {}).get("spec", {}).get("replicas") == 2 and (dc("custom") or {})
            .get("status", {}).get("availableReplicas") == 2), "the DC stays on custom-2")
st, out = make("noimage", {"type": "Custom", "customParams": {"command": ["x"]}}, 1)
check(st == 422 and "customParams.image: Required value" in json.dumps(out), f"a Custom strategy without an image is refused: {st}")
st, out = req("POST", f"/apis/apps.openshift.io/v1/namespaces/{NS}/deploymentconfigs", {
    "apiVersion": "apps.openshift.io/v1", "kind": "DeploymentConfig", "metadata": {"name": "img"},
    "spec": {"replicas": 1, "selector": {"app": "img"},
             "triggers": [{"type": "ImageChange", "imageChangeParams": {"automatic": True, "containerNames": ["web"],
                           "from": {"kind": "ImageStreamTag", "name": "web:latest"}}}],
             "template": {"metadata": {"labels": {"app": "img"}}, "spec": {"containers": [{"name": "web", "image": "x"}]}}}})
check(st == 422 and "ImageChange" in json.dumps(out), f"an ImageChange trigger is refused: {st}")

stop.set()
sys.exit(failed)
PY
report
