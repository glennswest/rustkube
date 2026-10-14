#!/usr/bin/env bash
#
# Three masters on one host (#149): a 3-member fastetcd Raft cluster, three
# apiservers against it, two electing controller-managers and two electing
# schedulers spread over the apiservers, a stand-in Node kept alive by its
# Lease. The failure matrix that fits on one machine:
#
#   - writes through one apiserver, reads through the others; a paged LIST
#     whose pages come from three different apiservers is one snapshot
#   - a watch continued through another apiserver from the last revision seen:
#     every event once, none lost
#   - the controller-manager leader paused (SIGSTOP) past its 15 s lease while
#     a Deployment is scaled: the standby takes over and scales; the resumed
#     old leader creates nothing extra (no duplicate ReplicaSets or Pods)
#   - the scheduler leader killed: the standby takes over; a node allowing 10
#     Pods is never given more than 10
#   - an apiserver killed: the control plane keeps working through the others
#     (a deleted Deployment's Pods are collected)
#   - one datastore member paused: writes still succeed (a minority)
#   - datastore quorum loss (two of three members killed): writes fail, and
#     nothing acknowledged is lost when the members return
#   - a fresh controller-manager rebuilds its queues and acts
#   - a master partitioned: apiserver C, whose controller-manager leads, cut
#     off from the datastore (C reaches fastetcd through a userspace proxy the
#     rig closes). C refuses writes with a 5xx; the leader's renewals fail and
#     the standby on B takes over and acts; healed, C serves current reads
#     and the old leader adds nothing
#
# Takeover times are printed; they are the intended lease-expiry failover,
# not request latency. A test pod has no privilege for netns or iptables, so
# the master partition is a TCP proxy between apiserver C and the datastore,
# and a stopped (SIGSTOP) process stands in for an unreachable one elsewhere.
# Separate machines' clocks and disks need hardware (#162). Exit status is the
# number of failed checks.
export RK_ETCD_MEMBERS=3 RK_APISERVERS=3
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_apiserver 1
# Apiserver C reaches the datastore through a proxy (one kernel-chosen
# loopback port per member) that drops every connection, and refuses new
# ones, while $W/partition exists: the master partition, without privilege.
cat >"$W/partition.py" <<'PROXY'
import asyncio, os, sys
flag, targets = sys.argv[1], [int(p) for p in sys.argv[2:]]
live = set()
async def pipe(r, w):
    try:
        while data := await r.read(65536):
            w.write(data); await w.drain()
    except Exception:
        pass
    finally:
        w.close()
def serve(target):
    async def handle(cr, cw):
        if os.path.exists(flag):
            cw.close(); return
        try:
            ur, uw = await asyncio.open_connection("127.0.0.1", target)
        except OSError:
            cw.close(); return
        pair = (cw, uw); live.add(pair)
        await asyncio.gather(pipe(cr, uw), pipe(ur, cw))
        live.discard(pair)
    return handle
async def main():
    servers = [await asyncio.start_server(serve(t), "127.0.0.1", 0) for t in targets]
    print(",".join(str(s.sockets[0].getsockname()[1]) for s in servers), flush=True)
    while True:
        await asyncio.sleep(0.2)
        if os.path.exists(flag):
            for pair in list(live):
                for w in pair: w.close()
asyncio.run(main())
PROXY
python3 -I "$W/partition.py" "$W/partition" $(for i in $(seq 0 $((MEMBERS - 1))); do printf '%d ' $((ETCD + 3 * i)); done) >"$W/proxy.ports" 2>"$W/proxy.log" &
PROXY_PID=$!
for _ in $(seq 50); do [ -s "$W/proxy.ports" ] && break; sleep 0.1; done
[ -s "$W/proxy.ports" ] || { echo "partition proxy did not start"; cat "$W/proxy.log"; exit 100; }
DIRECT_ETCD_SERVERS=$ETCD_SERVERS
proxied=$(<"$W/proxy.ports")
ETCD_SERVERS=$(printf 'http://127.0.0.1:%s,' ${proxied//,/ })
ETCD_SERVERS=${ETCD_SERVERS%,}
start_apiserver 2
ETCD_SERVERS=$DIRECT_ETCD_SERVERS
for n in 1 2; do
  for _ in $(seq 120); do
    curl -sfk -H "Authorization: Bearer $ADMIN" "https://127.0.0.1:$((PORT + n))/readyz" >/dev/null && break
    sleep 1
  done
done
export W BIN API ADMIN PORT ETCD FASTETCD CLUSTER
export API_PIDS_STR="${API_PIDS[*]}" STORE_PIDS_STR="${STORE_PIDS[*]}"
python3 - <<'PY' || FAIL=$?
import datetime, http.client, json, os, re, signal, ssl, subprocess, sys, threading, time, urllib.parse

W, BIN, ADMIN, PORT = os.environ["W"], os.environ["BIN"], os.environ["ADMIN"], int(os.environ["PORT"])
ETCD, FASTETCD, CLUSTER = int(os.environ["ETCD"]), os.environ["FASTETCD"], os.environ["CLUSTER"]
API_PIDS = [int(p) for p in os.environ["API_PIDS_STR"].split()]
STORE_PIDS = [int(p) for p in os.environ["STORE_PIDS_STR"].split()]
APIS = [f"https://127.0.0.1:{PORT + n}" for n in range(3)]
CTX = ssl._create_unverified_context()
failed = 0
def check(ok, what):
    global failed
    print(("PASS  " if ok else "FAIL  ") + what, flush=True)
    failed += 0 if ok else 1
    return ok

def diag(app):
    """Why a failover check failed, as NOTE lines: rigs.sh copies those to
    the pod log whole, where a failed rig's tail is mostly report()'s dumps of
    apiserver A and member 0, not the processes this rig started."""
    note = lambda m: print("NOTE  " + m[:600], flush=True)
    note(f"diag {app}: controller-manager lease {holder('kube-controller-manager')}, "
         f"scheduler lease {holder('kube-scheduler')}, procs alive "
         f"{sorted(k for k, p in procs.items() if p.poll() is None)}")
    for n in (1, 2):
        _, d = req(n, "GET", f"/apis/apps/v1/namespaces/mm/deployments/{app}")
        rs = req(n, "GET", f"/apis/apps/v1/namespaces/mm/replicasets?labelSelector=app%3D{app}")[1]
        ps = req(n, "GET", f"/api/v1/namespaces/mm/pods?labelSelector=app%3D{app}")[1]
        note(f"diag {app} via {'ABC'[n]}: deployment status {(d or {}).get('status') if isinstance(d, dict) else d}; "
             f"replicasets {[(r['metadata']['name'], r['spec'].get('replicas'), r.get('status', {}).get('replicas')) for r in rs['items']] if isinstance(rs, dict) else rs}; "
             f"pods {len(ps['items']) if isinstance(ps, dict) else ps}")
    for f in sorted(os.listdir(W)):
        if re.fullmatch(r"(controller-manager-.|scheduler-.|apiserver[12]|fastetcd[12])\.log", f):
            lines = [l.rstrip() for l in open(f"{W}/{f}", errors="replace")
                     if (" WARN " in l or " ERROR " in l) and "404 Not Found" not in l]
            for l in lines[-12:]:
                note(f"diag {f}: {l}")

def req(n, method, path, body=None, ctype="application/json", timeout=15):
    """(status, body) from apiserver n; (0, error) when it cannot answer."""
    try:
        conn = http.client.HTTPSConnection("127.0.0.1", PORT + n, context=CTX, timeout=timeout)
        conn.request(method, path, body=None if body is None else json.dumps(body),
                     headers={"Authorization": "Bearer " + ADMIN, "Content-Type": ctype})
        r = conn.getresponse(); raw = r.read(); conn.close()
        return r.status, json.loads(raw) if raw else None
    except Exception as e:
        return 0, str(e)

def until(f, limit):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        v = f()
        if v: return v
        time.sleep(0.3)
    return f()

def micro():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")

procs = {}
def launch(kind, name, n):
    log = open(f"{W}/{kind}-{name}.log", "w")
    procs[f"{kind}-{name}"] = subprocess.Popen(
        [f"{BIN}/kube-{kind}", "--apiserver", APIS[n],
         "--token", os.environ["CM_TOKEN" if kind == "controller-manager" else "SCHED_TOKEN"],
         "--certificate-authority", f"{W}/ca.crt", "--leader-elect", "true"],
        stdout=log, stderr=subprocess.STDOUT)
def identity(kind, name):
    def f():
        m = re.search(r"identity=(\S+?)\)", open(f"{W}/{kind}-{name}.log").read())
        return m.group(1) if m else None
    return until(f, 90)
def holder(lease):
    for n in range(3):
        code, l = req(n, "GET", f"/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/{lease}")
        if code == 200: return l["spec"].get("holderIdentity")
    return None

try:
    # --- the stand-in Node, kept Ready by its Lease -------------------------
    req(0, "POST", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases",
        {"apiVersion": "coordination.k8s.io/v1", "kind": "Lease", "metadata": {"name": "n1"},
         "spec": {"holderIdentity": "n1", "leaseDurationSeconds": 40, "renewTime": micro()}})
    def renew():
        while True:
            time.sleep(5)
            for n in (1, 2, 0):
                if req(n, "PATCH", "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/n1",
                       {"spec": {"renewTime": micro()}}, "application/merge-patch+json")[0] == 200:
                    break
    threading.Thread(target=renew, daemon=True).start()
    req(0, "POST", "/api/v1/nodes", {"apiVersion": "v1", "kind": "Node",
        "metadata": {"name": "n1", "labels": {"kubernetes.io/hostname": "n1"}}})
    _, node = req(0, "GET", "/api/v1/nodes/n1")
    res = {"cpu": "64", "memory": "256Gi", "pods": "10"}
    node["status"] = {"capacity": res, "allocatable": res, "conditions": [{"type": "Ready", "status": "True"}]}
    req(0, "PUT", "/api/v1/nodes/n1/status", node)
    req(0, "POST", "/api/v1/namespaces", {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "mm"}})

    # --- reads and writes across apiservers ---------------------------------
    for i in range(30):
        req(0, "POST", "/api/v1/namespaces/mm/configmaps",
            {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": f"cm{i:02}"}, "data": {"i": str(i)}})
    names = lambda n: {c["metadata"]["name"] for c in req(n, "GET", "/api/v1/namespaces/mm/configmaps")[1]["items"]}
    check(names(1) == names(2) == {f"cm{i:02}" for i in range(30)}, "30 writes through A are read through B and C")
    pages, revs, cont = [], set(), ""
    for n in (1, 2, 0):
        q = "?limit=10" + (f"&continue={urllib.parse.quote(cont, safe='')}" if cont else "")
        _, p = req(n, "GET", "/api/v1/namespaces/mm/configmaps" + q)
        pages += [c["metadata"]["name"] for c in p["items"]]
        revs.add(p["metadata"]["resourceVersion"]); cont = p["metadata"].get("continue", "")
    check(sorted(pages) == [f"cm{i:02}" for i in range(30)] and len(revs) == 1,
          f"a LIST paged through B, C, A: 30 objects once, one revision {revs}")

    # --- a watch continued through another apiserver ------------------------
    _, lst = req(1, "GET", "/api/v1/namespaces/mm/configmaps")
    rv = lst["metadata"]["resourceVersion"]
    def created(): return [f"w{i}" for i in range(10)]
    for name in created():
        req(2, "POST", "/api/v1/namespaces/mm/configmaps",
            {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name}})
    seen, last = [], rv
    for n, upto in ((1, 5), (0, 10)):
        conn = http.client.HTTPSConnection("127.0.0.1", PORT + n, context=CTX, timeout=15)
        conn.request("GET", f"/api/v1/namespaces/mm/configmaps?watch=true&resourceVersion={last}",
                     headers={"Authorization": "Bearer " + ADMIN})
        r = conn.getresponse()
        try:
            while len(seen) < upto:
                ev = json.loads(r.readline())
                seen.append(ev["object"]["metadata"]["name"]); last = ev["object"]["metadata"]["resourceVersion"]
        except Exception as e:
            print(f"watch via {n}: {e!r}")
        conn.close()
    check(seen == created(), f"a watch moved from B to A at its last revision: every event once, in order ({seen})")

    # --- electing controller-managers and schedulers ------------------------
    launch("controller-manager", "a", 0); launch("controller-manager", "b", 1)
    launch("scheduler", "a", 1); launch("scheduler", "b", 2)
    cms = {identity("controller-manager", "a"): "controller-manager-a", identity("controller-manager", "b"): "controller-manager-b"}
    scheds = {identity("scheduler", "a"): "scheduler-a", identity("scheduler", "b"): "scheduler-b"}
    cm_leader = until(lambda: holder("kube-controller-manager") in cms and holder("kube-controller-manager"), 60)
    sched_leader = until(lambda: holder("kube-scheduler") in scheds and holder("kube-scheduler"), 60)
    check(bool(cm_leader) and bool(sched_leader), f"one controller-manager and one scheduler lead ({cm_leader}, {sched_leader})")

    D = "/apis/apps/v1/namespaces/mm/deployments"
    def deploy(name, replicas):
        # The create's own answer is checked: a refused create (C still
        # catching up after quorum loss) is not "no controller-manager acted".
        code, body = req(2, "POST", D, {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name},
            "spec": {"replicas": replicas, "selector": {"matchLabels": {"app": name}},
                     "template": {"metadata": {"labels": {"app": name}},
                                  "spec": {"containers": [{"name": "c", "image": "unused"}]}}}})
        check(code == 201, f"Deployment {name} created through C ({code} {'' if code == 201 else body})")
    def scale(n, name, replicas):
        return req(n, "PATCH", f"{D}/{name}", {"spec": {"replicas": replicas}}, "application/merge-patch+json")[0]
    def pods(app, n=1):
        code, l = req(n, "GET", f"/api/v1/namespaces/mm/pods?labelSelector=app%3D{app}")
        return [p for p in l["items"] if not p["metadata"].get("deletionTimestamp")] if code == 200 else None
    def rsets(app, n=1):
        return [r for r in req(n, "GET", f"/apis/apps/v1/namespaces/mm/replicasets?labelSelector=app%3D{app}")[1]["items"]]
    deploy("d", 4)
    check(bool(until(lambda: (p := pods("d")) is not None and len(p) == 4 and all(x["spec"].get("nodeName") for x in p), 60)),
          "Deployment d: 4 Pods created and bound")

    # --- the controller-manager leader paused past its lease ----------------
    leader = procs[cms[cm_leader]]
    leader.send_signal(signal.SIGSTOP); t0 = time.monotonic()
    scale(1, "d", 7)
    new = until(lambda: (h := holder("kube-controller-manager")) not in (None, cm_leader) and h, 60)
    took = time.monotonic() - t0
    check(bool(new), f"standby controller-manager takes over from the paused leader ({took:.1f} s; lease 15 s)")
    if not check(bool(until(lambda: (p := pods("d")) is not None and len(p) == 7, 60)), "the new leader scales d to 7"):
        diag("d")
    leader.send_signal(signal.SIGCONT)
    time.sleep(20)
    p, r = pods("d"), rsets("d")
    check(p is not None and len(p) == 7 and len(r) == 1,
          f"the resumed old leader creates nothing: {len(p or [])} Pods, {len(r)} ReplicaSet(s)")

    # --- the scheduler leader killed; a 10-Pod node -------------------------
    procs[scheds[sched_leader]].kill(); t0 = time.monotonic()
    scale(1, "d", 12)
    new = until(lambda: (h := holder("kube-scheduler")) not in (None, sched_leader) and h, 60)
    check(bool(new), f"standby scheduler takes over ({time.monotonic() - t0:.1f} s)")
    until(lambda: (p := pods("d")) is not None and sum(1 for x in p if x["spec"].get("nodeName")) >= 10, 60)
    time.sleep(10)
    p = pods("d")
    bound = sum(1 for x in p if x["spec"].get("nodeName"))
    check(len(p) == 12 and bound == 10, f"12 Pods, 10 bound to the 10-Pod node, never more ({bound} bound)")

    # --- an apiserver killed ------------------------------------------------
    os.kill(API_PIDS[0], signal.SIGKILL)
    check(req(1, "DELETE", f"{D}/d")[0] in (200, 202), "Deployment d deleted through B with A dead")
    if not check(bool(until(lambda: (p := pods("d", 2)) is not None and not p, 120)),
                 "its ReplicaSet's Pods are collected with A dead (a controller-manager on B or C)"):
        diag("d")

    # --- one datastore member paused: a minority, writes go on --------------
    os.kill(STORE_PIDS[2], signal.SIGSTOP)
    ok = until(lambda: req(1, "POST", "/api/v1/namespaces/mm/configmaps",
                           {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "minority"}})[0] == 201, 60)
    check(bool(ok), "one of three members paused: a write through B still succeeds")
    os.kill(STORE_PIDS[2], signal.SIGCONT)

    # --- datastore quorum loss ----------------------------------------------
    before = names(1)
    for i in (1, 2):
        os.kill(STORE_PIDS[i], signal.SIGKILL)
    time.sleep(3)
    code, _ = req(1, "POST", "/api/v1/namespaces/mm/configmaps",
                  {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "during-loss"}}, timeout=30)
    # 5xx: the datastore's failure, not a 403 that reads as a permission
    # problem (namespace admission turned every read error into one).
    check(500 <= code < 600, f"no quorum: a write is refused with a 5xx, not acknowledged ({code})")
    for i in (1, 2):
        procs[f"fastetcd-{i}"] = subprocess.Popen([FASTETCD, "--name", f"m{i}", "--data-dir", f"{W}/etcd{i}",
            "--listen-client-urls", f"http://127.0.0.1:{ETCD + 3 * i}",
            "--listen-peer-urls", f"http://127.0.0.1:{ETCD + 3 * i + 1}",
            "--initial-advertise-peer-urls", f"http://127.0.0.1:{ETCD + 3 * i + 1}",
            "--listen-metrics-url", f"127.0.0.1:{ETCD + 3 * i + 2}",
            "--initial-cluster-token", "rig", "--initial-cluster", CLUSTER],
            stdout=open(f"{W}/fastetcd{i}.log", "a"), stderr=subprocess.STDOUT)
    ok = until(lambda: req(1, "POST", "/api/v1/namespaces/mm/configmaps",
                           {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "after-loss"}})[0] == 201, 120)
    check(bool(ok), "quorum back: writes succeed again")
    # C's first reads may still find its store connection catching up: a
    # client retries those, and so does this, timed.
    t0 = time.monotonic()
    listed = until(lambda: (r := req(2, "GET", "/api/v1/namespaces/mm/configmaps"))[0] == 200 and r[1], 60)
    if listed:
        after = {c["metadata"]["name"] for c in listed["items"]}
        check(before <= after, f"nothing acknowledged was lost ({len(before - after)} missing; "
                               f"C answered a LIST {time.monotonic() - t0:.1f} s after B's write)")
    else:
        check(False, f"C answers a LIST within 60 s of quorum ({req(2, 'GET', '/api/v1/namespaces/mm/configmaps')})")

    # --- a fresh controller-manager rebuilds and acts -----------------------
    for k in [k for k in procs if k.startswith("controller-manager")]:
        procs[k].kill()
    launch("controller-manager", "c", 2)
    deploy("e", 2)
    if not check(bool(until(lambda: (p := pods("e", 2)) is not None and len(p) == 2, 120)),
                 "a fresh controller-manager on C rebuilds its queues and creates e's 2 Pods"):
        diag("e")

    # --- a master partitioned from the datastore ----------------------------
    # C's controller-manager leads (it is the only one); a standby joins on B,
    # then C loses the datastore while it still answers its clients.
    c_id = identity("controller-manager", "c")
    until(lambda: holder("kube-controller-manager") == c_id, 60)
    launch("controller-manager", "d", 1)
    d_id = identity("controller-manager", "d")
    open(f"{W}/partition", "w").close(); t0 = time.monotonic()
    code, _ = req(2, "POST", "/api/v1/namespaces/mm/configmaps",
                  {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "partitioned"}}, timeout=30)
    check(500 <= code < 600, f"partitioned C refuses a write with a 5xx ({code})")
    new = until(lambda: holder("kube-controller-manager") == d_id, 90)
    check(bool(new), f"the standby on B takes over from the partitioned leader on C "
                     f"({time.monotonic() - t0:.1f} s; lease 15 s)")
    scale(1, "e", 4)
    if not check(bool(until(lambda: (p := pods("e")) is not None and len(p) == 4, 60)),
                 "the new leader on B scales e to 4 while C is partitioned"):
        diag("e")
    os.remove(f"{W}/partition")
    fresh = until(lambda: (p := pods("e", 2)) is not None and len(p) == 4, 60)
    check(bool(fresh), "healed: C serves e's 4 Pods (its watch cache caught up)")
    time.sleep(20)
    p, r = pods("e"), rsets("e")
    check(p is not None and len(p) == 4 and len(r) == 1 and holder("kube-controller-manager") == d_id,
          f"healed: the old leader adds nothing ({len(p or [])} Pods, {len(r)} ReplicaSet(s), "
          f"lease {holder('kube-controller-manager')})")
finally:
    for p in procs.values():
        try: p.send_signal(signal.SIGCONT); p.kill()
        except Exception: pass
sys.exit(failed)
PY
report
