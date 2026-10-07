#!/usr/bin/env bash
#
# networking.k8s.io/v1 ServiceCIDR and IPAddress served (#134), what the
# conformance suite's two "ServiceCIDR and IPAddress API" specs exercise. A
# real apiserver on fastetcd, no controllers.
#
# - discovery lists servicecidrs (+ /status) and ipaddresses, cluster-scoped
# - the bootstrapped `kubernetes` ServiceCIDR holds --service-cidr, Ready
# - ServiceCIDR: create, get, list, merge patch, /status update, delete
# - IPAddress: create (protobuf too: the schema exists now), get, list,
#   patch labels, delete
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import http.client, json, os, ssl, sys, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
G = "/apis/networking.k8s.io/v1"
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None, ctype="application/json", accept="application/json"):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)
    data = body if isinstance(body, (bytes, type(None))) else json.dumps(body)
    c.request(method, path, body=data, headers={"Authorization": "Bearer " + os.environ["ADMIN"],
                                                "Content-Type": ctype, "Accept": accept})
    r = c.getresponse(); raw = r.read(); ct = r.getheader("Content-Type", ""); c.close()
    try: return r.status, (json.loads(raw) if raw and "json" in ct else raw), ct
    except ValueError: return r.status, raw, ct

_, res, _ = req("GET", G)
by = {r["name"]: r for r in res["resources"]}
check(by.get("servicecidrs", {}).get("namespaced") is False and "servicecidrs/status" in by
      and by.get("ipaddresses", {}).get("namespaced") is False,
      f"discovery: servicecidrs (+/status) and ipaddresses, cluster-scoped ({sorted(by)})")

code, k, _ = req("GET", f"{G}/servicecidrs/kubernetes")
ready = [c for c in (k.get("status", {}).get("conditions") or []) if c["type"] == "Ready"] if code == 200 else []
check(code == 200 and k["spec"]["cidrs"] == ["10.96.0.0/12"] and ready and ready[0]["status"] == "True",
      f"bootstrapped kubernetes ServiceCIDR from --service-cidr, Ready ({code} {k if code != 200 else k['spec']})")

code, sc, _ = req("POST", f"{G}/servicecidrs", {"apiVersion": "networking.k8s.io/v1", "kind": "ServiceCIDR",
    "metadata": {"name": "extra"}, "spec": {"cidrs": ["10.100.0.0/24"]}})
check(code == 201 and sc["kind"] == "ServiceCIDR", f"create ServiceCIDR ({code})")
code, l, _ = req("GET", f"{G}/servicecidrs")
check(code == 200 and l["kind"] == "ServiceCIDRList" and {i["metadata"]["name"] for i in l["items"]} >= {"kubernetes", "extra"},
      f"list ServiceCIDRs ({code})")
code, p, _ = req("PATCH", f"{G}/servicecidrs/extra", {"metadata": {"labels": {"e2e": "patched"}}}, "application/merge-patch+json")
check(code == 200 and p["metadata"]["labels"]["e2e"] == "patched", f"merge patch ({code})")
p["status"] = {"conditions": [{"type": "Ready", "status": "True", "reason": "e2e", "message": "ok",
                               "lastTransitionTime": "2026-10-07T00:00:00Z"}]}
code, s, _ = req("PUT", f"{G}/servicecidrs/extra/status", p)
check(code == 200 and s["status"]["conditions"][0]["reason"] == "e2e", f"/status update ({code})")
code, _, _ = req("DELETE", f"{G}/servicecidrs/extra")
check(code == 200 and req("GET", f"{G}/servicecidrs/extra")[0] == 404, f"delete ServiceCIDR ({code})")

ip = {"apiVersion": "networking.k8s.io/v1", "kind": "IPAddress", "metadata": {"name": "10.100.0.5"},
      "spec": {"parentRef": {"group": "", "resource": "services", "namespace": "default", "name": "web"}}}
code, out, _ = req("POST", f"{G}/ipaddresses", ip)
check(code == 201 and out["spec"]["parentRef"]["name"] == "web", f"create IPAddress ({code})")
code, out, ct = req("GET", f"{G}/ipaddresses/10.100.0.5", accept="application/vnd.kubernetes.protobuf")
check(code == 200 and "protobuf" in ct, f"IPAddress over protobuf: {code} {ct}")
code, l, _ = req("GET", f"{G}/ipaddresses")
check(code == 200 and l["kind"] == "IPAddressList" and any(i["metadata"]["name"] == "10.100.0.5" for i in l["items"]),
      f"list IPAddresses ({code})")
code, p, _ = req("PATCH", f"{G}/ipaddresses/10.100.0.5", {"metadata": {"labels": {"e2e": "x"}}}, "application/merge-patch+json")
check(code == 200 and p["metadata"]["labels"]["e2e"] == "x", f"patch IPAddress ({code})")
code, _, _ = req("DELETE", f"{G}/ipaddresses/10.100.0.5")
check(code == 200 and req("GET", f"{G}/ipaddresses/10.100.0.5")[0] == 404, f"delete IPAddress ({code})")
sys.exit(failed)
PY
report
