#!/usr/bin/env bash
#
# EndpointSliceMirroring (#133) against a real apiserver + controller-manager
# on fastetcd, no kubelet. The conformance spec "should mirror a custom
# Endpoints resource through create update and delete", and around it:
#
# - a selectorless Service + hand-written Endpoints (10.1.2.3:80) → one
#   IPv4 slice labelled service-name + managed-by mirroring, owned by the
#   Endpoints, with the Endpoints' labels
# - Endpoints updated to 10.2.3.4 → the same slice, the new address
# - an IPv6 address added → a second slice, addressType IPv6
# - Endpoints deleted → the mirrored slices go
# - Endpoints labelled skip-mirror → nothing mirrored
# - the Service given a selector → its mirrored slices go; with the
#   hand-written Endpoints deleted, the controller's own carry skip-mirror
# - Service deleted → its mirrored slices go
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
export API ADMIN
python3 - <<'PY' || FAIL=$?
import json, os, ssl, time, urllib.request, urllib.error
base, token = os.environ['API'], os.environ['ADMIN']
context = ssl._create_unverified_context()
failed = 0
def request(method, path, body=None):
    req = urllib.request.Request(base + path, method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, context=context, timeout=10) as r:
        return json.load(r)
def get(path):
    try: return request('GET', path)
    except urllib.error.HTTPError as e:
        if e.code == 404: return None
        raise
def check(ok, label):
    global failed
    print(('PASS  ' if ok else 'FAIL  ') + label, flush=True); failed += 0 if ok else 1
def eventually(label, test, secs=20):
    until = time.monotonic() + secs
    while time.monotonic() < until:
        if test(): return check(True, label)
        time.sleep(.2)
    check(False, label + ' — slices: ' + json.dumps(slices('example-custom-endpoints')))
NS = '/api/v1/namespaces/mirror'
ES = '/apis/discovery.k8s.io/v1/namespaces/mirror/endpointslices'
request('POST', '/api/v1/namespaces', {'apiVersion': 'v1', 'kind': 'Namespace', 'metadata': {'name': 'mirror'}})
def slices(svc):
    sel = 'kubernetes.io%2Fservice-name%3D' + svc + '%2Cendpointslice.kubernetes.io%2Fmanaged-by%3Dendpointslicemirroring-controller.k8s.io'
    return request('GET', ES + '?labelSelector=' + sel)['items']
def endpoints(ip, labels=None, extra=()):
    addrs = [{'ip': ip}] + [{'ip': x} for x in extra]
    return {'apiVersion': 'v1', 'kind': 'Endpoints',
            'metadata': {'name': 'example-custom-endpoints', 'labels': labels or {'app': 'mirror'}},
            'subsets': [{'addresses': addrs, 'ports': [{'name': 'example', 'port': 80, 'protocol': 'TCP'}]}]}
svc = request('POST', NS + '/services', {'apiVersion': 'v1', 'kind': 'Service', 'metadata': {'name': 'example-custom-endpoints'},
    'spec': {'ports': [{'name': 'example', 'port': 80, 'targetPort': 80}]}})
ep = request('POST', NS + '/endpoints', endpoints('10.1.2.3'))
def one(ip):
    s = slices('example-custom-endpoints')
    return len(s) == 1 and s[0]['addressType'] == 'IPv4' and [e['addresses'] for e in s[0]['endpoints']] == [[ip]] \
        and s[0]['ports'] == [{'name': 'example', 'port': 80, 'protocol': 'TCP'}]
eventually('create: one mirrored IPv4 slice with 10.1.2.3:80', lambda: one('10.1.2.3'))
s = slices('example-custom-endpoints')[0] if slices('example-custom-endpoints') else {'metadata': {}}
check(s['metadata'].get('ownerReferences', [{}])[0].get('uid') == ep['metadata']['uid']
      and s['metadata'].get('ownerReferences', [{}])[0].get('kind') == 'Endpoints', 'owned by the Endpoints')
check(s['metadata'].get('labels', {}).get('app') == 'mirror', "the Endpoints' labels copied")
first = s['metadata'].get('name')
cur = get(NS + '/endpoints/example-custom-endpoints')
upd = endpoints('10.2.3.4'); upd['metadata']['resourceVersion'] = cur['metadata']['resourceVersion']
request('PUT', NS + '/endpoints/example-custom-endpoints', upd)
eventually('update: the slice carries 10.2.3.4', lambda: one('10.2.3.4'))
check(slices('example-custom-endpoints')[0]['metadata']['name'] == first, 'same slice updated, not replaced')
cur = get(NS + '/endpoints/example-custom-endpoints')
upd = endpoints('10.2.3.4', extra=['fd00::5']); upd['metadata']['resourceVersion'] = cur['metadata']['resourceVersion']
request('PUT', NS + '/endpoints/example-custom-endpoints', upd)
eventually('an IPv6 address: a second slice', lambda: sorted(x['addressType'] for x in slices('example-custom-endpoints')) == ['IPv4', 'IPv6'])
request('DELETE', NS + '/endpoints/example-custom-endpoints')
eventually('delete: the mirrored slices go', lambda: slices('example-custom-endpoints') == [])

request('POST', NS + '/endpoints', endpoints('10.3.4.5', labels={'endpointslice.kubernetes.io/skip-mirror': 'true'}))
time.sleep(3)
check(slices('example-custom-endpoints') == [], 'skip-mirror: nothing mirrored')
cur = get(NS + '/endpoints/example-custom-endpoints')
upd = endpoints('10.3.4.5'); upd['metadata']['resourceVersion'] = cur['metadata']['resourceVersion']
request('PUT', NS + '/endpoints/example-custom-endpoints', upd)
eventually('skip-mirror removed: mirrored', lambda: one('10.3.4.5'))
cur = get(NS + '/services/example-custom-endpoints')
cur['spec']['selector'] = {'app': 'nothing'}
request('PUT', NS + '/services/example-custom-endpoints', cur)
eventually('the Service gains a selector: mirrored slices go', lambda: slices('example-custom-endpoints') == [])
# The hand-written Endpoints is not the controller's to overwrite; once it is
# gone the controller writes its own.
request('DELETE', NS + '/endpoints/example-custom-endpoints')
eventually("the controller's Endpoints carry skip-mirror",
           lambda: (get(NS + '/endpoints/example-custom-endpoints') or {}).get('metadata', {}).get('labels', {})
                   .get('endpointslice.kubernetes.io/skip-mirror') == 'true')

request('POST', NS + '/services', {'apiVersion': 'v1', 'kind': 'Service', 'metadata': {'name': 'other'},
    'spec': {'ports': [{'port': 80}]}})
e = endpoints('10.4.5.6'); e['metadata']['name'] = 'other'
request('POST', NS + '/endpoints', e)
eventually('second selectorless Service mirrored', lambda: len(slices('other')) == 1)
request('DELETE', NS + '/services/other')
eventually('Service deleted: its mirrored slices go', lambda: slices('other') == [])
raise SystemExit(failed)
PY
report
