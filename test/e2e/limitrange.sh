#!/usr/bin/env bash
#
# LimitRanger admission (#131), with the conformance spec's LimitRange, on a
# real apiserver on fastetcd (no kubelet: only what admission writes is read).
#
# - a Pod with no resources gets defaultRequest/default (100m/200Mi/200Gi
#   requests, 500m/500Mi/500Gi limits) and the kubernetes.io/limit-ranger
#   annotation
# - a Pod with partial resources: the cpu request is its limit (300m), the
#   rest merged from the LimitRange
# - below min, above max: 403 naming the bound
# - the LimitRange relaxed (min 9m): a Pod that was refused is admitted
# - a PersistentVolumeClaim against a type: PersistentVolumeClaim min: 403
# - a Pod with limits only, in a namespace without LimitRanges, gets
#   requests equal to its limits
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": "application/json"})
    r = c.getresponse(); raw = r.read(); c.close()
    return r.status, json.loads(raw) if raw else None
def rl(cpu, mem, eph):
    return {k: v for k, v in (("cpu", cpu), ("memory", mem), ("ephemeral-storage", eph)) if v}
req("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "lr"}})
NS = "/api/v1/namespaces/lr"
code, lr = req("POST", f"{NS}/limitranges", {"apiVersion": "v1", "kind": "LimitRange", "metadata": {"name": "limit-range"},
    "spec": {"limits": [{"type": "Container", "min": rl("50m", "100Mi", "100Gi"), "max": rl("500m", "500Mi", "500Gi"),
                          "default": rl("500m", "500Mi", "500Gi"), "defaultRequest": rl("100m", "200Mi", "200Gi")},
                         {"type": "PersistentVolumeClaim", "min": {"storage": "1Gi"}}]}})
check(code == 201, f"LimitRange created ({code})")
def pod(name, requests, limits):
    return req("POST", f"{NS}/pods", {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name},
        "spec": {"containers": [{"name": "pause", "image": "pause", "resources": {"requests": requests, "limits": limits}}]}})
code, p = pod("pod-no-resources", {}, {})
r = p["spec"]["containers"][0]["resources"] if code == 201 else {}
check(code == 201 and r.get("requests") == rl("100m", "200Mi", "200Gi") and r.get("limits") == rl("500m", "500Mi", "500Gi"),
      f"no resources: defaults applied ({code} {r})")
check(code == 201 and "LimitRanger plugin set" in p["metadata"].get("annotations", {}).get("kubernetes.io/limit-ranger", ""),
      "annotated kubernetes.io/limit-ranger")
code, p = pod("pod-partial-resources", rl("", "150Mi", "150Gi"), rl("300m", "", ""))
r = p["spec"]["containers"][0]["resources"] if code == 201 else {}
check(code == 201 and r.get("requests") == rl("300m", "150Mi", "150Gi") and r.get("limits") == rl("300m", "500Mi", "500Gi"),
      f"partial resources merged ({code} {r})")
code, out = pod("below", rl("10m", "50Mi", "50Gi"), {})
check(code == 403 and "minimum cpu usage per Container is 50m, but request is 10m" in out.get("message", ""), f"below min: 403 ({code} {out.get('message')})")
code, out = pod("above", rl("600m", "600Mi", "600Gi"), {})
check(code == 403 and "maximum cpu usage per Container is 500m" in out.get("message", ""), f"above max: 403 ({code})")
lr["spec"]["limits"][0]["min"] = rl("9m", "49Mi", "49Gi")
req("PUT", f"{NS}/limitranges/limit-range", lr)
code, _ = pod("below", rl("10m", "50Mi", "50Gi"), {})
check(code == 201, f"min relaxed to 9m: the 10m Pod is admitted ({code})")
code, out = req("POST", f"{NS}/persistentvolumeclaims", {"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": "small"},
    "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "500Mi"}}}})
check(code == 403 and "minimum storage usage per PersistentVolumeClaim is 1Gi" in out.get("message", ""), f"PVC below min: 403 ({code})")
code, p = req("POST", "/api/v1/namespaces/default/pods", {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "lim-only"},
    "spec": {"containers": [{"name": "c", "image": "i", "resources": {"limits": {"cpu": "1", "memory": "1Gi"}}}]}})
check(code == 201 and p["spec"]["containers"][0]["resources"].get("requests") == {"cpu": "1", "memory": "1Gi"}
      and p["status"].get("qosClass") == "Guaranteed",
      f"no LimitRange: requests default to limits, QoS Guaranteed ({code})")
sys.exit(failed)
PY
report
