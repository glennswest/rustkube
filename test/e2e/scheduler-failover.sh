#!/usr/bin/env bash
# Scheduler leadership and dependency wakeups on a disposable API/store rig
# (#145), without a kubelet or controller-manager:
#   - two electing schedulers; the leader is paused (SIGSTOP) past its lease,
#     the standby takes over, and the resumed former leader binds nothing
#   - the standby is killed and the former leader takes over again, rebuilding
#     its reservations from bound Pods: a one-CPU Node is never overcommitted
#   - a Pod waiting for a missing claim binds when the claim appears, onto the
#     node its PersistentVolume requires; a WaitForFirstConsumer-style claim
#     gets selected-node first and the Pod binds when the claim is bound
. "$(dirname "$0")/lib.sh"
export API ADMIN BIN W
python3 - <<'PY'
import json, os, re, signal, ssl, subprocess, time, urllib.request, urllib.error
base, token = os.environ['API'], os.environ['ADMIN']; W = os.environ['W']
context = ssl._create_unverified_context()
def req(method, path, body=None, ctype='application/json'):
    r = urllib.request.Request(base+path, method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={'Authorization': 'Bearer '+token, 'Content-Type': ctype})
    with urllib.request.urlopen(r, context=context, timeout=10) as s: return json.load(s)
def get(path):
    try: return req('GET', path)
    except urllib.error.HTTPError as e:
        if e.code == 404: return None
        raise
checks = 0
def ok(label):
    global checks; checks += 1; print('PASS', label, flush=True)
def wait(label, test, limit):
    start = time.monotonic()
    while time.monotonic() < start+limit:
        if test():
            ok(f'{label} ({time.monotonic()-start:.2f}s)'); return time.monotonic()-start
        time.sleep(.05)
    raise AssertionError(f'{label}: not within {limit}s')
def hold(label, test, seconds):
    end = time.monotonic()+seconds
    while time.monotonic() < end:
        assert test(), label
        time.sleep(.2)
    ok(label)

procs = {}
def launch(name):
    log = open(f'{W}/sched-{name}.log', 'w')
    procs[name] = subprocess.Popen([os.environ['BIN']+'/kube-scheduler', '--apiserver', base,
        '--token', token, '--certificate-authority', W+'/ca.crt', '--leader-elect', 'true'],
        stdout=log, stderr=subprocess.STDOUT)
def identity(name):
    end = time.monotonic()+60
    while time.monotonic() < end:
        m = re.search(r'identity=(\S+?)\)', open(f'{W}/sched-{name}.log').read())
        if m: return m.group(1)
        time.sleep(.1)
    raise AssertionError(f'scheduler {name} never started election')
def holder():
    lease = get('/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/kube-scheduler')
    return (lease or {}).get('spec', {}).get('holderIdentity')

ns = '/api/v1/namespaces/failover'
def pods(): return {p['metadata']['name']: p for p in req('GET', ns+'/pods')['items']}
def bound_on(node):
    return sorted(n for n, p in pods().items() if p['spec'].get('nodeName') == node
                  and p.get('status', {}).get('phase') not in ('Succeeded', 'Failed'))
def finish(name):
    p = get(ns+'/pods/'+name); p.setdefault('status', {})['phase'] = 'Succeeded'
    req('PUT', ns+'/pods/'+name+'/status', p)
def pod(name, selector=None, claim=None, cpu='600m', on=None):
    spec = {'containers': [{'name': 'c', 'image': 'unused',
            'resources': {'requests': {'cpu': cpu, 'memory': '64Mi'}}}]}
    if on: spec['nodeName'] = on
    if selector: spec['nodeSelector'] = selector
    if claim: spec['volumes'] = [{'name': 'v', 'persistentVolumeClaim': {'claimName': claim}}]
    req('POST', ns+'/pods', {'apiVersion': 'v1', 'kind': 'Pod', 'metadata': {'name': name}, 'spec': spec})
def node(name, labels, cpu):
    n = req('POST', '/api/v1/nodes', {'apiVersion': 'v1', 'kind': 'Node',
            'metadata': {'name': name, 'labels': labels}})
    res = {'cpu': cpu, 'memory': '8Gi', 'pods': '100'}
    n['status'] = {'capacity': res, 'allocatable': res,
                   'conditions': [{'type': 'Ready', 'status': 'True'}]}
    req('PUT', f'/api/v1/nodes/{name}/status', n)

try:
    req('POST', '/api/v1/namespaces', {'metadata': {'name': 'failover'}})
    node('capacity-one', {'pool': 'one', 'kubernetes.io/hostname': 'capacity-one'}, '1')
    one = {'pool': 'one'}

    launch('a'); a = identity('a')
    wait('scheduler A leads', lambda: holder() == a, 60)
    launch('b'); b = identity('b')
    assert a != b, 'identities must be unique per process'
    time.sleep(4)
    hold('standby B does not take a renewed lease', lambda: holder() == a, 6)

    pod('p1', one)
    wait('leader A binds p1', lambda: bound_on('capacity-one') == ['p1'], 5)
    pod('p2', one); pod('p3', one)
    hold('A leaves p2/p3 pending on a full one-CPU Node', lambda: bound_on('capacity-one') == ['p1'], 3)

    procs['a'].send_signal(signal.SIGSTOP)
    wait('B takes over from paused A (lease 15 s)', lambda: holder() == b, 45)
    assert bound_on('capacity-one') == ['p1'], 'overcommitted during takeover'
    ok('no overcommit across takeover')

    finish('p1')
    wait('B wakes on released capacity and binds one of p2/p3',
         lambda: len(bound_on('capacity-one')) == 1 and bound_on('capacity-one') != ['p1'], 10)
    placed = bound_on('capacity-one'); waiting = ({'p2', 'p3'} - set(placed)).pop()
    hold('only one Pod holds the released CPU', lambda: bound_on('capacity-one') == placed, 2)

    resumed = time.monotonic()
    procs['a'].send_signal(signal.SIGCONT)
    hold('resumed stale leader A binds nothing and B keeps the lease',
         lambda: bound_on('capacity-one') == placed and holder() == b, 8)

    procs['b'].kill(); procs['b'].wait()
    wait('A takes over from killed B', lambda: holder() == a, 45)
    hold('A rebuilds reservations from bound Pods: no overcommit',
         lambda: bound_on('capacity-one') == placed, 3)
    finish(placed[0])
    wait(f'A binds {waiting} once capacity is released', lambda: bound_on('capacity-one') == [waiting], 10)
    hold('released CPU spent exactly once', lambda: bound_on('capacity-one') == [waiting], 2)

    # Dependency wakeups for storage.
    # The volume's node is the busier one (10% CPU free against 40% on
    # capacity-one), so least-requested scoring picks capacity-one first while
    # v1's claim is missing. A reservation left from that attempt would pin v1
    # there and it could never bind to its volume's node.
    node('volume-node', {'pool': 'vol', 'kubernetes.io/hostname': 'volume-node'}, '2')
    pod('filler', cpu='1800m', on='volume-node')
    pod('v1', claim='late', cpu='100m')
    hold('v1 waits for its missing claim', lambda: not pods()['v1']['spec'].get('nodeName'), 2)
    req('POST', '/api/v1/persistentvolumes', {'apiVersion': 'v1', 'kind': 'PersistentVolume',
        'metadata': {'name': 'late-pv'}, 'spec': {'capacity': {'storage': '1Gi'},
        'accessModes': ['ReadWriteOnce'], 'hostPath': {'path': '/nonexistent'},
        'nodeAffinity': {'required': {'nodeSelectorTerms': [{'matchExpressions': [
            {'key': 'kubernetes.io/hostname', 'operator': 'In', 'values': ['volume-node']}]}]}}}})
    req('POST', ns+'/persistentvolumeclaims', {'apiVersion': 'v1', 'kind': 'PersistentVolumeClaim',
        'metadata': {'name': 'late'}, 'spec': {'accessModes': ['ReadWriteOnce'], 'storageClassName': '',
        'volumeName': 'late-pv', 'resources': {'requests': {'storage': '1Gi'}}}})
    wait('claim creation wakes v1', lambda: bool(pods()['v1']['spec'].get('nodeName')), 5)
    assert pods()['v1']['spec']['nodeName'] == 'volume-node', 'v1 ignored its volume node affinity'
    ok('v1 bound where its PersistentVolume is')

    req('POST', ns+'/persistentvolumeclaims', {'apiVersion': 'v1', 'kind': 'PersistentVolumeClaim',
        'metadata': {'name': 'wffc'}, 'spec': {'accessModes': ['ReadWriteOnce'], 'storageClassName': '',
        'resources': {'requests': {'storage': '1Gi'}}}})
    pod('v2', {'pool': 'vol'}, claim='wffc', cpu='100m')
    sel = 'volume.kubernetes.io/selected-node'
    claim = lambda: get(ns+'/persistentvolumeclaims/wffc')
    wait('unbound claim gets selected-node', lambda: claim()['metadata'].get('annotations', {}).get(sel) == 'volume-node', 5)
    hold('v2 is not bound before its claim is', lambda: not pods()['v2']['spec'].get('nodeName'), 2)
    req('POST', '/api/v1/persistentvolumes', {'apiVersion': 'v1', 'kind': 'PersistentVolume',
        'metadata': {'name': 'wffc-pv'}, 'spec': {'capacity': {'storage': '1Gi'},
        'accessModes': ['ReadWriteOnce'], 'hostPath': {'path': '/nonexistent'}}})
    c = claim(); c['spec']['volumeName'] = 'wffc-pv'
    req('PUT', ns+'/persistentvolumeclaims/wffc', c)
    wait('claim binding wakes v2', lambda: pods()['v2']['spec'].get('nodeName') == 'volume-node', 5)
    print(f'{checks} scheduler failover/wakeup checks passed', flush=True)
finally:
    for p in procs.values():
        if p.poll() is None:
            p.send_signal(signal.SIGCONT); p.kill(); p.wait()
PY
if [ "$?" -ne 0 ]; then
  fail 'scheduler failover and wakeups'
  for s in a b; do echo "---- scheduler $s log (tail)"; tail -40 "$W/sched-$s.log" 2>/dev/null; done
fi
report
