#!/usr/bin/env bash
# Bounded Service/PDB workers against the real API + fastetcd. No kubelet needed.
. "$(dirname "$0")/lib.sh"
start_controller_manager
export API ADMIN
python3 - <<'PY'
import json, os, ssl, time, urllib.request, urllib.error
base, token = os.environ['API'], os.environ['ADMIN']
context = ssl._create_unverified_context()
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
checks = 0
def eventually(label, test):
    global checks
    until = time.monotonic() + 20
    while time.monotonic() < until:
        if test():
            print('PASS', label, flush=True); checks += 1; return
        time.sleep(.1)
    raise AssertionError(label)
def create(path, kind, name, spec, **extra):
    return request('POST', path, dict(apiVersion='v1' if '/api/v1/' in path else 'policy/v1',
        kind=kind, metadata={'name':name}, spec=spec, **extra))
for ns in ['indexed-a','indexed-b']:
    create('/api/v1/namespaces', 'Namespace', ns, {})
ns = '/api/v1/namespaces/indexed-a'
pdbpath = '/apis/policy/v1/namespaces/indexed-a/poddisruptionbudgets'
svc = create(ns+'/services', 'Service','web', {'selector':{'app':'web'},'ports':[{'port':80}]})
create(pdbpath, 'PodDisruptionBudget','web', {'selector':{'matchLabels':{'app':'web'}},'minAvailable':0})
create(pdbpath, 'PodDisruptionBudget','negative', {'selector':{'matchExpressions':[{'key':'app','operator':'NotIn','values':['web']}]},'minAvailable':0})
def pod(namespace):
    return request('POST', '/api/v1/namespaces/'+namespace+'/pods', {'apiVersion':'v1','kind':'Pod',
        'metadata':{'name':'p','labels':{'app':'web'}}, 'spec':{'containers':[{'name':'c','image':'unused'}]},
        'status':{'phase':'Running','podIP':'10.99.0.1','conditions':[{'type':'Ready','status':'True'}]}})
p = pod('indexed-a'); pod('indexed-b')
ep = ns+'/endpoints/web'
es = '/apis/discovery.k8s.io/v1/namespaces/indexed-a/endpointslices/web'
def count(path, field):
    o = get(path)
    if o is None: return -1
    if field == 'subsets': return sum(len(s.get('addresses',[])) for s in o.get('subsets',[]))
    if field == 'endpoints': return len(o.get('endpoints',[]))
    return o.get('status',{}).get('expectedPods',-1)
eventually('Service sees one Pod, never the other namespace', lambda:count(ep,'subsets') == 1)
eventually('EndpointSlice mirrors membership', lambda:count(es,'endpoints') == 1)
eventually('PDB sees one Pod', lambda:count(pdbpath+'/web','status') == 1)
eventually('negative selector excludes web Pod', lambda:count(pdbpath+'/negative','status') == 0)
p = get(ns+'/pods/p'); p['metadata']['labels']['app']='api'; request('PUT',ns+'/pods/p',p)
eventually('old labels wake Service', lambda:count(ep,'subsets') == 0)
eventually('old labels wake PDB', lambda:count(pdbpath+'/web','status') == 0)
eventually('negative selector gains relabelled Pod', lambda:count(pdbpath+'/negative','status') == 1)
p = get(ns+'/pods/p'); p['metadata']['labels']['app']='web'; request('PUT',ns+'/pods/p',p)
eventually('new labels restore endpoints', lambda:count(ep,'subsets') == 1)
eventually('new labels restore PDB', lambda:count(pdbpath+'/web','status') == 1)
time.sleep(1)
paths = [ep,es,pdbpath+'/web',pdbpath+'/negative']
versions = [get(path)['metadata']['resourceVersion'] for path in paths]
time.sleep(2)
assert versions == [get(path)['metadata']['resourceVersion'] for path in paths], 'idle status/write loop'
print('PASS stable state does not write', flush=True); checks += 1
p = get(ns+'/pods/p'); request('DELETE',ns+'/pods/p',{'gracePeriodSeconds':0,'preconditions':{'uid':p['metadata']['uid']}})
eventually('Pod deletion removes old membership', lambda:count(ep,'subsets') == 0 and count(pdbpath+'/web','status') == 0)
request('DELETE',ns+'/services/web',{'preconditions':{'uid':svc['metadata']['uid']}})
eventually('Service deletion collects only its owned endpoints', lambda:get(ep) is None and get(es) is None)
create(ns+'/services','Service','web',{'selector':{'app':'web'},'ports':[{'port':80}]})
eventually('recreated Service gets new owned endpoints', lambda:get(ep) is not None and get(es) is not None)
assert get(ep)['metadata']['ownerReferences'][0]['uid'] != svc['metadata']['uid']
print('PASS replacement Service uses new UID', flush=True); checks += 1
print(f'{checks} indexed selector checks passed', flush=True)
PY
[ "$?" -eq 0 ] || fail 'indexed selector regression'
report
