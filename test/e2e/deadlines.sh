#!/usr/bin/env bash
# Semantic deadlines on a disposable API/store rig, without a kubelet (#144).
#
# The controllers have no poll loops: every time-driven action is an explicit
# requeue for the moment it becomes due. Each check here arranges a deadline a
# few seconds ahead with nothing else changing, and requires the action to
# happen after the deadline (not early) and soon after it (a wake, not a slow
# sweep). The last check counts the controller-manager's API requests over a
# quiet window: an idle control plane should make none.
. "$(dirname "$0")/lib.sh"
start_controller_manager
export API ADMIN
python3 - <<'PY'
import datetime, json, os, ssl, threading, time, urllib.request, urllib.error, uuid
base, token = os.environ['API'], os.environ['ADMIN']; context = ssl._create_unverified_context()
def req(method, path, body=None):
    r = urllib.request.Request(base + path, method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    with urllib.request.urlopen(r, context=context, timeout=10) as s: return json.load(s)
def get(path):
    try: return req('GET', path)
    except urllib.error.HTTPError as e:
        if e.code == 404: return None
        raise
def iso(t, micro=False):
    d = datetime.datetime.fromtimestamp(t, datetime.timezone.utc)
    return d.strftime('%Y-%m-%dT%H:%M:%S.%fZ' if micro else '%Y-%m-%dT%H:%M:%SZ')
def parse(s): return datetime.datetime.strptime(s[:19], '%Y-%m-%dT%H:%M:%S').replace(tzinfo=datetime.timezone.utc).timestamp()
failed = 0; passed = 0
def check(label, ok, detail=''):
    global failed, passed
    print(('PASS ' if ok else 'FAIL ') + label + (f' ({detail})' if detail else ''), flush=True)
    if ok: passed += 1
    else: failed += 1
def until(test, limit):
    end = time.time() + limit
    while time.time() < end:
        v = test()
        if v: return v, time.time()
        time.sleep(.1)
    return None, time.time()
# Timestamps are whole seconds: allow one second either side of a deadline.
def on_time(label, fired_at, due, late=6):
    if fired_at is None: return check(label, False, 'never happened')
    lag = fired_at - due
    check(label, -1.0 <= lag <= late, f'{lag:+.1f}s after the deadline')

# Deadlines fall while other checks run: each is watched by its own thread,
# which records when the action was first seen.
fired = {}
def watch(name, test, limit):
    def run():
        v, at = until(test, limit)
        fired[name] = (v, at if v else None)
    t = threading.Thread(target=run); t.start(); return t

req('POST', '/api/v1/namespaces', {'metadata': {'name': 'deadlines'}})
ns = '/api/v1/namespaces/deadlines'
batch = '/apis/batch/v1/namespaces/deadlines'
template = {'spec': {'restartPolicy': 'Never', 'containers': [{'name': 'c', 'image': 'unused'}]}}

# --- Event retention: a not-yet-expired Event goes at its expiry (TTL 1 h).
expiry = time.time() + 10
req('POST', ns + '/events', {'apiVersion': 'v1', 'kind': 'Event', 'metadata': {'name': 'expiring'},
    'lastTimestamp': iso(expiry - 3600), 'reason': 'Test', 'message': 'expires soon',
    'involvedObject': {'kind': 'Pod', 'namespace': 'deadlines', 'name': 'none', 'uid': 'none'}})
expiry = parse(iso(expiry - 3600)) + 3600

# --- Node heartbeat: a Lease that stops renewing taints its Node (grace 40 s).
renewed = time.time() - 30
req('POST', '/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases',
    {'apiVersion': 'coordination.k8s.io/v1', 'kind': 'Lease', 'metadata': {'name': 'stale-lease'},
     'spec': {'holderIdentity': 'stale-lease', 'leaseDurationSeconds': 40, 'renewTime': iso(renewed, True)}})
node = req('POST', '/api/v1/nodes', {'apiVersion': 'v1', 'kind': 'Node', 'metadata': {'name': 'stale-lease'}})
node['status'] = {'conditions': [{'type': 'Ready', 'status': 'True'}],
    'capacity': {'cpu': '1', 'memory': '1Gi', 'pods': '10'}, 'allocatable': {'cpu': '1', 'memory': '1Gi', 'pods': '10'}}
req('PUT', '/api/v1/nodes/stale-lease/status', node)
heartbeat_due = renewed + 40

# --- Job activeDeadlineSeconds.
req('POST', batch + '/jobs', {'apiVersion': 'batch/v1', 'kind': 'Job', 'metadata': {'name': 'deadline'},
    'spec': {'activeDeadlineSeconds': 8, 'backoffLimit': 0, 'template': template}})

# --- GC fails closed: an owner of a kind nobody serves cannot be proven absent.
req('POST', ns + '/secrets', {'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': 'unknown-owner',
    'ownerReferences': [{'apiVersion': 'example.test/v1', 'kind': 'Widget', 'name': 'w', 'uid': str(uuid.uuid4())}]}})
req('POST', ns + '/secrets', {'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': 'missing-owner',
    'ownerReferences': [{'apiVersion': 'v1', 'kind': 'ConfigMap', 'name': 'gone', 'uid': str(uuid.uuid4())}]}})

# --- ReplicaSet recreation backoff (10 s after a Failed Pod).
rs = req('POST', '/apis/apps/v1/namespaces/deadlines/replicasets', {'apiVersion': 'apps/v1', 'kind': 'ReplicaSet',
    'metadata': {'name': 'backoff'}, 'spec': {'replicas': 1, 'selector': {'matchLabels': {'app': 'backoff'}},
    'template': {'metadata': {'labels': {'app': 'backoff'}}, 'spec': template['spec'] | {'restartPolicy': 'Always'}}}})
def active_rs_pods():
    return [p for p in req('GET', ns + '/pods?labelSelector=app%3Dbackoff')['items']
            if p.get('status', {}).get('phase') not in ('Failed', 'Succeeded') and not p['metadata'].get('deletionTimestamp')]

def tainted():
    n = get('/api/v1/nodes/stale-lease')
    return any(t.get('key') == 'node.kubernetes.io/not-ready' and t.get('effect') == 'NoExecute'
               for t in n.get('spec', {}).get('taints', []))
def job_failed():
    j = get(batch + '/jobs/deadline')
    return j if j and any(c.get('type') == 'Failed' and c.get('reason') == 'DeadlineExceeded'
                          for c in j.get('status', {}).get('conditions', [])) else None
threads = [watch('event', lambda: get(ns + '/events/expiring') is None, 40),
           watch('node', tainted, 40),
           watch('job', job_failed, 40)]

# The GC control case settles first.
_, _ = until(lambda: get(ns + '/secrets/missing-owner') is None, 30)
check('GC deletes a dependent whose owner is proven absent', get(ns + '/secrets/missing-owner') is None)

for round in (1, 2):
    pods, _ = until(active_rs_pods, 30)
    if not pods: check(f'ReplicaSet round {round} has a Pod', False); break
    pod = pods[0]; pod.setdefault('status', {})['phase'] = 'Failed'
    req('PUT', ns + '/pods/' + pod['metadata']['name'] + '/status', pod)
    failed_at = time.time()
    replaced, at = until(lambda: [p for p in active_rs_pods() if p['metadata']['uid'] != pod['metadata']['uid']], 45)
    lag = at - failed_at
    check(f'ReplicaSet replaces Failed Pod {round} after its backoff', bool(replaced) and 8 <= lag <= 20,
          f'{lag:.1f}s' if replaced else 'never replaced')

for t in threads: t.join()
on_time('Event deleted at its TTL expiry', fired['event'][1], expiry)
on_time('stale Lease taints its Node at the grace deadline', fired['node'][1], heartbeat_due)
job = fired['job'][0] or get(batch + '/jobs/deadline')
start = job.get('status', {}).get('startTime')
on_time('Job fails at activeDeadlineSeconds', fired['job'][1], parse(start) + 8 if start else 0)

check('GC keeps a dependent whose owner kind is not served', get(ns + '/secrets/unknown-owner') is not None)

# --- CronJob: the first run starts at its minute.
cj = req('POST', batch + '/cronjobs', {'apiVersion': 'batch/v1', 'kind': 'CronJob', 'metadata': {'name': 'minutely'},
    'spec': {'schedule': '* * * * *', 'jobTemplate': {'spec': {'template': template}}}})
created = parse(cj['metadata']['creationTimestamp'])
minute = (int(created) // 60 + 1) * 60
def cron_job():
    return [j for j in req('GET', batch + '/jobs')['items']
            if any(o.get('uid') == cj['metadata']['uid'] for o in j['metadata'].get('ownerReferences', []))]
jobs, at = until(cron_job, minute - time.time() + 20)
on_time('CronJob starts its Job at the scheduled minute', at if jobs else None, minute)
req('DELETE', batch + '/cronjobs/minutely', {'propagationPolicy': 'Background'})
req('DELETE', '/api/v1/nodes/stale-lease', {})
print(f'{passed} deadline checks passed, {failed} failed', flush=True)
raise SystemExit(1 if failed else 0)
PY
[ "$?" -eq 0 ] || fail 'deadline checks'

# --- No idle polling: with nothing changing, the controller-manager makes no
# API requests. Watches are long-lived and not counted.
scrape() {
  curl --max-time 5 -sk "$API/metrics" | grep '^apiserver_request_total{' | grep -v 'verb="watch"'
}
sleep 10   # let the cleanup above settle
scrape >"$W/idle.before"
sleep 20
scrape >"$W/idle.after"
# The first scrape itself completes inside the window and is counted once.
idle=$(awk 'NR==FNR { b[$1]=$2; next } { d=$2-b[$1]; if (d>0) { n+=d; print "  +" d, $1 > "/dev/stderr" } } END { printf "%d\n", n-1 }' \
  "$W/idle.before" "$W/idle.after")
if [ "$idle" -le 5 ]; then pass "idle control plane: $idle API requests in 20 s"
else fail "idle control plane made $idle API requests in 20 s"; fi
report
