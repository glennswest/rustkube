#!/usr/bin/env bash
#
# Startup manifests get the API's admission and defaulting (#158), against a
# real apiserver on fastetcd started with --manifest-dir, then restarted with
# changed Reconcile manifests.
#
# First boot, created from manifests:
# - a Pod: default ServiceAccount + token volume, not-ready/unreachable
#   tolerations, QoS class, phase Pending, LimitRange defaults (and its
#   annotation) from a LimitRange earlier in the pass
# - a NodePort Service without a clusterIP: a ClusterIP and a node port
# - a Secret: stringData folded into data
# - a ConfigMap with an invalid key: not created (as the API refuses it)
# - a custom resource of a CRD in the same pass: schema default applied,
#   generation 1
# Second boot (Reconcile manifests changed):
# - the Service reconciled without a clusterIP keeps its address and port
# - an immutable ConfigMap's data change is refused (not applied)
#
# Exit status is the number of failed checks.
MD=$(mktemp -d "${TMPDIR:-/tmp}/rk-manifests.XXXXXX")

cat >"$MD/10-ns.yaml" <<'Y'
apiVersion: v1
kind: Namespace
metadata: {name: boot}
---
apiVersion: v1
kind: LimitRange
metadata: {name: lr, namespace: boot}
spec:
  limits:
  - type: Container
    default: {cpu: 500m, memory: 128Mi}
    defaultRequest: {cpu: 100m, memory: 64Mi}
Y
cat >"$MD/20-objects.yaml" <<'Y'
apiVersion: v1
kind: Pod
metadata: {name: p, namespace: boot}
spec:
  containers: [{name: c, image: pause}]
---
apiVersion: v1
kind: Service
metadata:
  name: np
  namespace: boot
  annotations: {addonmanager.kubernetes.io/mode: Reconcile}
spec:
  type: NodePort
  ports: [{port: 80}]
---
apiVersion: v1
kind: Secret
metadata: {name: s, namespace: boot}
stringData: {k: v}
---
apiVersion: v1
kind: ConfigMap
metadata: {name: bad, namespace: boot}
data: {"no/slash": x}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: frozen
  namespace: boot
  annotations: {addonmanager.kubernetes.io/mode: Reconcile}
immutable: true
data: {a: "1"}
Y
cat >"$MD/30-crd.yaml" <<'Y'
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: widgets.boot.example.com}
spec:
  group: boot.example.com
  scope: Namespaced
  names: {plural: widgets, singular: widget, kind: Widget, listKind: WidgetList}
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            properties:
              size: {type: integer, default: 3}
Y
cat >"$MD/40-cr.yaml" <<'Y'
apiVersion: boot.example.com/v1
kind: Widget
metadata: {name: w, namespace: boot}
spec: {}
Y
RK_APISERVER_ARGS="--manifest-dir $MD"
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
check_py() {
python3 - "$1" <<'PY'
import http.client, json, os, ssl, sys, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def get(path):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    c.request("GET", path, headers={"Authorization": "Bearer " + os.environ["ADMIN"]})
    r = c.getresponse(); raw = r.read(); c.close()
    return r.status, (json.loads(raw) if raw else None)
NS = "/api/v1/namespaces/boot"
if sys.argv[1] == "first":
    code, p = get(NS + "/pods/p")
    c = p["spec"]["containers"][0] if code == 200 else {}
    check(code == 200 and p["spec"].get("serviceAccountName") == "default", f"Pod: default ServiceAccount ({code})")
    check(code == 200 and any(v["name"].startswith("kube-api-access-") for v in p["spec"].get("volumes", [])), "Pod: token volume")
    keys = {t.get("key") for t in p["spec"].get("tolerations", [])} if code == 200 else set()
    check({"node.kubernetes.io/not-ready", "node.kubernetes.io/unreachable"} <= keys, f"Pod: default tolerations ({keys})")
    check(code == 200 and c["resources"].get("requests") == {"cpu": "100m", "memory": "64Mi"}
          and c["resources"].get("limits") == {"cpu": "500m", "memory": "128Mi"}
          and "kubernetes.io/limit-ranger" in p["metadata"].get("annotations", {}), f"Pod: LimitRange defaults ({c.get('resources')})")
    check(code == 200 and p["status"].get("qosClass") == "Burstable" and p["status"].get("phase") == "Pending",
          f"Pod: QoS and phase ({p['status'] if code == 200 else ''})")
    code, s = get(NS + "/services/np")
    check(code == 200 and s["spec"].get("clusterIP") and 30000 <= s["spec"]["ports"][0].get("nodePort", 0) <= 32767,
          f"Service: ClusterIP and node port allocated ({s['spec'] if code == 200 else code})")
    open(os.environ["W"] + "/np.json", "w").write(json.dumps(s["spec"] if code == 200 else {}))
    code, sec = get(NS + "/secrets/s")
    check(code == 200 and sec.get("data", {}).get("k") == "dg==" and "stringData" not in sec, f"Secret: stringData folded ({sec if code == 200 else code})")
    code, _ = get(NS + "/configmaps/bad")
    check(code == 404, f"ConfigMap with an invalid key not created ({code})")
    code, w = get("/apis/boot.example.com/v1/namespaces/boot/widgets/w")
    check(code == 200 and w["spec"].get("size") == 3 and w["metadata"].get("generation") == 1,
          f"CR: schema default and generation ({w if code == 200 else code})")
else:
    before = json.loads(open(os.environ["W"] + "/np.json").read())
    code, s = get(NS + "/services/np")
    check(code == 200 and s["spec"].get("clusterIP") == before.get("clusterIP")
          and s["spec"]["ports"][0].get("nodePort") == before["ports"][0].get("nodePort")
          and s["spec"]["ports"][0].get("port") == 8080 and s["spec"]["ports"][0].get("targetPort") == 8080,
          f"Service reconciled: port changed (targetPort defaulted), address and node port kept ({s['spec'] if code == 200 else code})")
    code, cm = get(NS + "/configmaps/frozen")
    check(code == 200 and cm.get("data") == {"a": "1"}, f"immutable ConfigMap: the reconcile was refused ({cm.get('data') if cm else code})")
sys.exit(failed)
PY
}
export MD W
check_py first || FAIL=$((FAIL + $?))
# Second boot: the Service without a clusterIP and a new port; the immutable
# ConfigMap with new data.
sed -i 's/ports: \[{port: 80}\]/ports: [{port: 8080}]/; s/data: {a: "1"}/data: {a: "2"}/' "$MD/20-objects.yaml"
kill "$API_PID"; wait "$API_PID" 2>/dev/null
start_apiserver 0; API_PID=${API_PIDS[0]}
for _ in $(seq 120); do
  curl -sfk -H "Authorization: Bearer $ADMIN" "$API/readyz" >/dev/null && break
  sleep 1
done
check_py second || FAIL=$((FAIL + $?))
grep -q 'ConfigMap frozen.*was not applied' "$W/apiserver.log" && pass "the refused reconcile is logged" || fail "the refused reconcile is logged"
rm -rf "$MD"
report
