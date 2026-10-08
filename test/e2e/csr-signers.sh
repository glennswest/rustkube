#!/usr/bin/env bash
#
# The CSR controller signs only its own signers and leaves an external
# signer's alone (#199), against a real apiserver and controller-manager on
# fastetcd. The controller-manager signs with the rig's CA and is told
# `--csr-external-signer-names kubernetes.io/kubelet-serving`.
#
# - kube-apiserver-client-kubelet: auto-approved and signed
# - kubelet-serving (external): not approved; approved by hand, not signed
# - stormcert.io/workload (another signer's): approved by hand, not signed
# - kube-apiserver-client: not auto-approved; approved by hand, signed
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
RK_CM_ARGS="--cluster-signing-cert-file $W/ca.crt --cluster-signing-key-file $W/ca.key --csr-external-signer-names kubernetes.io/kubelet-serving"
start_controller_manager
for n in kc ks sc ac; do
  openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$W/$n.key" -out "$W/$n.csr" \
    -subj "/O=system:nodes/CN=system:node:n1" 2>/dev/null || exit 100
done
export API ADMIN W
python3 - <<'PY' || FAIL=$?
import base64, http.client, json, os, ssl, sys, time, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context(); W = os.environ["W"]
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
P = "/apis/certificates.k8s.io/v1/certificatesigningrequests"
def csr(name, file, signer, usages):
    pem = base64.b64encode(open(f"{W}/{file}.csr", "rb").read()).decode()
    return req("POST", P, {"apiVersion": "certificates.k8s.io/v1", "kind": "CertificateSigningRequest", "metadata": {"name": name},
                           "spec": {"request": pem, "signerName": signer, "usages": usages}})
def get(name): return req("GET", f"{P}/{name}")[1]
def approved(o): return any(c["type"] == "Approved" for c in o.get("status", {}).get("conditions", []))
def signed(o): return bool(o.get("status", {}).get("certificate"))
def approve(name):
    o = get(name)
    o.setdefault("status", {})["conditions"] = [{"type": "Approved", "status": "True", "reason": "E2E", "message": "by hand"}]
    return req("PUT", f"{P}/{name}/approval", o)[0]
def until(f, secs=20):
    end = time.monotonic() + secs
    while time.monotonic() < end:
        if f(): return True
        time.sleep(0.3)
    return f()
client = ["digital signature", "client auth"]
csr("kubelet-client", "kc", "kubernetes.io/kube-apiserver-client-kubelet", client)
csr("kubelet-serving", "ks", "kubernetes.io/kubelet-serving", ["digital signature", "server auth"])
csr("stormcert", "sc", "stormcert.io/workload", client)
csr("api-client", "ac", "kubernetes.io/kube-apiserver-client", client)
check(until(lambda: approved(get("kubelet-client")) and signed(get("kubelet-client"))), "kubelet client: auto-approved and signed")
time.sleep(4)
check(not approved(get("kubelet-serving")), "kubelet-serving (external): not approved here")
check(not approved(get("api-client")), "kube-apiserver-client: not auto-approved")
for n in ("kubelet-serving", "stormcert", "api-client"):
    approve(n)
check(until(lambda: signed(get("api-client"))), "kube-apiserver-client approved by hand: signed")
time.sleep(4)
check(not signed(get("kubelet-serving")), "kubelet-serving (external) approved by hand: not signed here")
check(not signed(get("stormcert")), "stormcert.io/workload (another signer's): not signed")
sys.exit(failed)
PY
report
