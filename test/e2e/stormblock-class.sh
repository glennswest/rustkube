#!/usr/bin/env bash
#
# The in-kubelet stormblock provisioner acts on its own class only (#92),
# against a real apiserver and controller-manager on fastetcd.
#
# - a StorageClass named `stormblock` whose provisioner is a CSI driver's
#   (csi.stormblock.io): a claim of it, with its node selected, gets no
#   in-kubelet PV
# - the class recreated with provisioner stormblock.storm.io (stormcos's):
#   the same claim gets PV pvc-<ns>-<claim>, annotated as this path's
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, time, urllib.parse
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
    try: return r.status, json.loads(raw)
    except ValueError: return r.status, raw
def until(f, secs=20):
    end = time.monotonic() + secs
    while time.monotonic() < end:
        v = f()
        if v: return v
        time.sleep(0.3)
    return f()
SC = "/apis/storage.k8s.io/v1/storageclasses"
def klass(provisioner):
    return req("POST", SC, {"apiVersion": "storage.k8s.io/v1", "kind": "StorageClass", "metadata": {"name": "stormblock"},
                            "provisioner": provisioner, "volumeBindingMode": "WaitForFirstConsumer"})
klass("csi.stormblock.io")
time.sleep(2)
req("POST", "/api/v1/namespaces/default/persistentvolumeclaims", {"apiVersion": "v1", "kind": "PersistentVolumeClaim",
    "metadata": {"name": "data", "annotations": {"volume.kubernetes.io/selected-node": "n1"}},
    "spec": {"storageClassName": "stormblock", "accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}}})
time.sleep(6)
check(req("GET", "/api/v1/persistentvolumes/pvc-default-data")[0] == 404,
      "a `stormblock` class with a CSI provisioner: no in-kubelet PV")
req("DELETE", SC + "/stormblock")
klass("stormblock.storm.io")
pv = until(lambda: (lambda c, o: o if c == 200 else None)(*req("GET", "/api/v1/persistentvolumes/pvc-default-data")))
check(pv is not None and pv["metadata"]["annotations"].get("pv.kubernetes.io/provisioned-by") == "stormblock.storm.io/in-kubelet",
      f"provisioner stormblock.storm.io: the claim gets its PV ({pv['metadata'] if pv else None})")
sys.exit(failed)
PY
report
