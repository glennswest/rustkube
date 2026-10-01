#!/usr/bin/env bash
#
# GET of one object is as fast as the LIST it belongs to (#177), for a
# ServiceAccount that RBAC has to authorize (cilium-operator's shape) as
# well as for system:masters, idle and under concurrent load: forty other
# identities renewing their own Leases and listing, as kubelets and
# controllers do. Also: datastore reads per authorized request, and a
# metadata-only (as=PartialObjectMetadata) CRD watch decodes line by line.
#
#   test/e2e/get-latency.sh           # from the checkout; builds what it runs
#
# RK_GET_P99_MS (default 50) is the p99 bound on a debug build on the build
# box. Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

SA_SUB=system:serviceaccount:kube-system:cilium-operator
SA_TOKEN=$(token "$SA_SUB" '["system:serviceaccounts","system:serviceaccounts:kube-system"]')
LOAD_TOKENS=$W/load-tokens
for i in $(seq 40); do token "system:serviceaccount:load:sa$i" '["system:serviceaccounts","system:serviceaccounts:load"]'; echo; done >"$LOAD_TOKENS"

API="$API" ADMIN="$ADMIN" SA_TOKEN="$SA_TOKEN" LOAD_TOKENS="$LOAD_TOKENS" \
  P99_MS="${RK_GET_P99_MS:-50}" python3 - <<'PY'
import http.client, json, os, ssl, sys, threading, time, urllib.parse

API = urllib.parse.urlparse(os.environ["API"])
ADMIN, SA = os.environ["ADMIN"], os.environ["SA_TOKEN"]
LOAD = [l.strip() for l in open(os.environ["LOAD_TOKENS"]) if l.strip()]
P99_MS = float(os.environ["P99_MS"])
CTX = ssl._create_unverified_context()
failed = 0

def conn():
    return http.client.HTTPSConnection(API.hostname, API.port, context=CTX, timeout=30)

class Client:
    """One keep-alive connection, as client-go holds one."""
    def __init__(self, token): self.token, self.c = token, conn()
    def req(self, method, path, body=None, accept="application/json"):
        h = {"Authorization": "Bearer " + self.token, "Accept": accept}
        if body is not None:
            h["Content-Type"] = "application/json"; body = json.dumps(body)
        for attempt in (0, 1):
            try:
                self.c.request(method, path, body=body, headers=h)
                r = self.c.getresponse(); data = r.read()
                return r.status, data
            except (http.client.HTTPException, OSError):
                if attempt: raise
                self.c.close(); self.c = conn()

def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    if not ok: failed += 1

admin = Client(ADMIN)
def must(method, path, body=None):
    s, d = admin.req(method, path, body)
    if s >= 300 and s != 409:
        print(f"setup {method} {path}: {s} {d[:300]!r}"); sys.exit(100)
    return d

# --- cilium-operator's RBAC, and a cluster's worth of other bindings ---------
must("POST", "/apis/rbac.authorization.k8s.io/v1/clusterroles", {
    "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
    "metadata": {"name": "cilium-operator"},
    "rules": [
        {"apiGroups": ["coordination.k8s.io"], "resources": ["leases"], "verbs": ["create", "get", "update", "list", "watch"]},
        {"apiGroups": ["apiextensions.k8s.io"], "resources": ["customresourcedefinitions"], "verbs": ["create", "get", "update", "list", "watch"]},
        {"apiGroups": [""], "resources": ["pods"], "verbs": ["get", "list", "watch"]}]})
must("POST", "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings", {
    "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding",
    "metadata": {"name": "cilium-operator"},
    "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "cilium-operator"},
    "subjects": [{"kind": "ServiceAccount", "name": "cilium-operator", "namespace": "kube-system"}]})
must("POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "load"}})
must("POST", "/apis/rbac.authorization.k8s.io/v1/clusterroles", {
    "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
    "metadata": {"name": "lease-holder"},
    "rules": [{"apiGroups": ["coordination.k8s.io"], "resources": ["leases"], "verbs": ["*"]}]})
for i in range(80):
    must("POST", "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings", {
        "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding",
        "metadata": {"name": f"filler-{i}"},
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "view"},
        "subjects": [{"kind": "User", "name": f"someone-{i}"}]})
for i in range(len(LOAD)):
    must("POST", "/apis/rbac.authorization.k8s.io/v1/namespaces/load/rolebindings", {
        "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
        "metadata": {"name": f"sa{i+1}", "namespace": "load"},
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "lease-holder"},
        "subjects": [{"kind": "ServiceAccount", "name": f"sa{i+1}", "namespace": "load"}]})

LEASE = "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases"
must("POST", LEASE, {"apiVersion": "coordination.k8s.io/v1", "kind": "Lease",
    "metadata": {"name": "cilium-operator-resource-lock"}, "spec": {"holderIdentity": "x"}})
CRDS = "/apis/apiextensions.k8s.io/v1/customresourcedefinitions"
for i in range(12):
    must("POST", CRDS, {"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
        "metadata": {"name": f"thing{i}s.rig.example.com"},
        "spec": {"group": "rig.example.com", "scope": "Namespaced",
            "names": {"plural": f"thing{i}s", "singular": f"thing{i}", "kind": f"Thing{i}", "listKind": f"Thing{i}List"},
            "versions": [{"name": "v1", "served": True, "storage": True,
                "schema": {"openAPIV3Schema": {"type": "object", "x-kubernetes-preserve-unknown-fields": True}}}]}})

PROBES = [
    ("sa", SA, "GET lease", LEASE + "/cilium-operator-resource-lock"),
    ("sa", SA, "LIST leases", LEASE),
    ("sa", SA, "GET crd", CRDS + "/thing3s.rig.example.com"),
    ("admin", ADMIN, "GET lease", LEASE + "/cilium-operator-resource-lock"),
    ("admin", ADMIN, "LIST leases", LEASE),
]

def pct(xs, p): xs = sorted(xs); return xs[min(len(xs) - 1, int(len(xs) * p))]

def measure(phase, n=200):
    out = {}
    for who, tok, what, path in PROBES:
        c, ms = Client(tok), []
        for _ in range(n):
            t = time.perf_counter(); s, _ = c.req("GET", path); ms.append((time.perf_counter() - t) * 1000)
            if s != 200: check(False, f"{phase}: {who} {what}: HTTP {s}"); break
        out[(who, what)] = ms
        print(f"      {phase:5} {who:5} {what:12} p50 {pct(ms, .5):7.1f} ms  p99 {pct(ms, .99):7.1f} ms  max {max(ms):7.1f} ms", flush=True)
    return out

def store_counts():
    s, d = admin.req("GET", "/metrics")
    counts = {}
    for line in d.decode().splitlines():
        if line.startswith("etcd_request_duration_seconds_count{"):
            labels, v = line.rsplit(" ", 1)
            counts[labels] = float(v)
    return counts

def reads_per_request(path, n=50):
    c = Client(SA); c.req("GET", path)
    before = store_counts()
    for _ in range(n): c.req("GET", path)
    after = store_counts()
    delta = {k: after[k] - before.get(k, 0) for k in after if after[k] - before.get(k, 0) > 0}
    # /metrics itself reads nothing from the store.
    return sum(delta.values()) / n, {k: v / n for k, v in delta.items()}

per, detail = reads_per_request(LEASE + "/cilium-operator-resource-lock")
print(f"      datastore calls per SA GET of a Lease: {per:.2f} ({detail})", flush=True)
check(per <= 1.05, f"an authorized GET makes one datastore call ({per:.2f})")

idle = measure("idle")

# --- load: forty identities renewing their own Lease and listing ------------
stop = threading.Event()
load_ops = [0]
def worker(i, tok):
    c = Client(tok); path = f"/apis/coordination.k8s.io/v1/namespaces/load/leases/l{i}"
    c.req("POST", "/apis/coordination.k8s.io/v1/namespaces/load/leases",
          {"apiVersion": "coordination.k8s.io/v1", "kind": "Lease", "metadata": {"name": f"l{i}"}, "spec": {}})
    n = 0
    while not stop.is_set():
        s, d = c.req("GET", path)
        if s == 200:
            obj = json.loads(d); obj["spec"]["renewTime"] = f"2026-10-01T00:00:{n % 60:02d}.000000Z"
            c.req("PUT", path, obj)
        if n % 5 == 0: c.req("GET", "/apis/coordination.k8s.io/v1/namespaces/load/leases")
        n += 1; load_ops[0] += 1
threads = [threading.Thread(target=worker, args=(i, t), daemon=True) for i, t in enumerate(LOAD)]
for t in threads: t.start()
time.sleep(3)
t0, ops0 = time.time(), load_ops[0]
loaded = measure("load")
rate = (load_ops[0] - ops0) / (time.time() - t0)
stop.set()
for t in threads: t.join(timeout=30)
print(f"      background: {len(LOAD)} clients, {rate:.0f} renew loops/s", flush=True)

for phase, res in (("idle", idle), ("load", loaded)):
    for who in ("sa", "admin"):
        g, l = res[(who, "GET lease")], res[(who, "LIST leases")]
        check(pct(g, .99) < P99_MS, f"{phase}: {who} GET lease p99 {pct(g, .99):.1f} ms < {P99_MS:.0f} ms")
        # The GET is never slower than the LIST it is a member of (+5 ms of jitter).
        check(pct(g, .5) <= pct(l, .5) + 5, f"{phase}: {who} GET p50 {pct(g, .5):.1f} ms <= LIST p50 {pct(l, .5):.1f} ms")
    c = res[("sa", "GET crd")]
    check(pct(c, .99) < P99_MS, f"{phase}: sa GET crd p99 {pct(c, .99):.1f} ms < {P99_MS:.0f} ms")

# --- metadata-only CRD watch decodes ----------------------------------------
w = conn()
w.request("GET", CRDS + "?watch=true&allowWatchBookmarks=true&resourceVersion=0",
          headers={"Authorization": "Bearer " + SA,
                   "Accept": "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1,application/json"})
r = w.getresponse()
check(r.status == 200, f"metadata-only CRD watch opens ({r.status})")
must("POST", CRDS, {"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
    "metadata": {"name": "lates.rig.example.com"},
    "spec": {"group": "rig.example.com", "scope": "Cluster",
        "names": {"plural": "lates", "singular": "late", "kind": "Late", "listKind": "LateList"},
        "versions": [{"name": "v1", "served": True, "storage": True,
            "schema": {"openAPIV3Schema": {"type": "object", "x-kubernetes-preserve-unknown-fields": True}}}]}})
bad, seen, buf, deadline = [], set(), b"", time.time() + 20
w.sock.settimeout(2)
while time.time() < deadline and "lates.rig.example.com" not in seen:
    try:
        chunk = r.read1(65536)
    except (TimeoutError, OSError):
        continue
    if not chunk: break
    buf += chunk
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        if not line.strip(): continue
        try:
            ev = json.loads(line); o = ev["object"]
            ok = ev["type"] in ("ADDED", "MODIFIED", "DELETED", "BOOKMARK") and \
                 o.get("apiVersion") == "meta.k8s.io/v1" and o.get("kind") == "PartialObjectMetadata" and \
                 isinstance(o.get("metadata"), dict) and "spec" not in o
            if not ok: bad.append(line[:200])
            seen.add(o.get("metadata", {}).get("name"))
        except Exception as e:
            bad.append(line[:200] + f" ({e})".encode())
check(not bad, f"every metadata-only watch event is a PartialObjectMetadata ({len(bad)} bad: {bad[:2]})")
check(len([n for n in seen if n and n.endswith("rig.example.com")]) == 13,
      f"metadata-only watch saw all 13 CRDs, the late one live ({len(seen)} names)")
sys.exit(min(failed, 99))
PY
FAIL=$?
[ "$FAIL" = 100 ] && exit 100
report
