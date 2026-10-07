#!/usr/bin/env bash
#
# Service create cost (#113): a real apiserver on fastetcd. ClusterIPs were
# claimed by walking the range from the bottom, one store write per Service
# already there, so creates took ~1.5 s, ran one at a time under
# concurrency, and slowed as Services accumulated.
#
# - 100 sequential creates: each well under a second, and the last 20 no
#   slower than twice the first 20 (no growth with the count)
# - 25 concurrent creates: all 201, all done within 10 s
# - 125 Services, 125 distinct ClusterIPs, every dynamic one outside the
#   bottom band upstream leaves for fixed addresses (/12: first 256)
# - a fixed ClusterIP in that band is still granted; asking for a taken one
#   is refused
# - a ConfigMap create for scale
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY' || FAIL=$?
import concurrent.futures, http.client, ipaddress, json, os, ssl, statistics, sys, time, urllib.parse
API = urllib.parse.urlparse(os.environ["API"]); CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True); failed += 0 if ok else 1
def req(method, path, body=None):
    c = http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=60)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={"Authorization": "Bearer " + os.environ["ADMIN"], "Content-Type": "application/json"})
    r = c.getresponse(); raw = r.read(); c.close()
    return r.status, json.loads(raw) if raw else None
S = "/api/v1/namespaces/default/services"
def svc(name, ip=None):
    spec = {"ports": [{"port": 80}]}
    if ip: spec["clusterIP"] = ip
    t = time.monotonic()
    code, out = req("POST", S, {"apiVersion": "v1", "kind": "Service", "metadata": {"name": name}, "spec": spec})
    return code, out, time.monotonic() - t

t = time.monotonic(); code, _ = req("POST", "/api/v1/namespaces/default/configmaps",
    {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "scale"}}); cm = time.monotonic() - t
print(f"INFO  ConfigMap create {cm*1000:.1f} ms", flush=True)

seq = []
for i in range(100):
    code, out, dt = svc(f"seq-{i}")
    if code != 201: check(False, f"seq-{i}: {code} {out}"); break
    seq.append(dt)
if len(seq) == 100:
    first, last = statistics.mean(seq[:20]), statistics.mean(seq[-20:])
    print(f"INFO  sequential: p50 {statistics.median(seq)*1000:.1f} ms, max {max(seq)*1000:.1f} ms, "
          f"first 20 mean {first*1000:.1f} ms, last 20 {last*1000:.1f} ms", flush=True)
    check(max(seq) < 1.0, f"100 sequential creates each under 1 s (max {max(seq):.3f} s)")
    check(last <= 2 * first + 0.01, f"no growth with the count: last 20 {last*1000:.1f} ms vs first 20 {first*1000:.1f} ms")

t0 = time.monotonic()
with concurrent.futures.ThreadPoolExecutor(25) as pool:
    results = list(pool.map(lambda i: svc(f"burst-{i}"), range(25)))
wall = time.monotonic() - t0
print(f"INFO  25 concurrent: wall {wall:.2f} s, slowest {max(r[2] for r in results):.2f} s", flush=True)
check(all(r[0] == 201 for r in results), f"25 concurrent creates all 201 ({[r[0] for r in results if r[0] != 201]})")
check(wall < 10, f"25 concurrent creates done within 10 s ({wall:.2f} s)")

_, lst = req("GET", S)
ips = [s["spec"]["clusterIP"] for s in lst["items"] if s["metadata"]["name"].startswith(("seq-", "burst-"))]
check(len(ips) == 125 and len(set(ips)) == 125, f"125 Services, 125 distinct ClusterIPs ({len(ips)}, {len(set(ips))})")
net = ipaddress.ip_network("10.96.0.0/12")
low = [ip for ip in ips if int(ipaddress.ip_address(ip)) - int(net.network_address) < 256]
check(not low, f"dynamic addresses stay out of the bottom 256 ({low[:5]})")

code, out, _ = svc("fixed", "10.96.0.53")
check(code == 201 and out["spec"]["clusterIP"] == "10.96.0.53", f"a fixed address in the band is granted ({code})")
code, out, _ = svc("fixed-again", "10.96.0.53")
check(code == 422 and "already allocated" in json.dumps(out), f"a taken fixed address is refused ({code})")
sys.exit(failed)
PY
report
