#!/usr/bin/env bash
# LIST contents and resourceVersion must describe the same snapshot. No controllers.
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY'
import concurrent.futures, json, os, ssl, threading, time, urllib.request, urllib.parse
base, token = os.environ['API'], os.environ['ADMIN']
context = ssl._create_unverified_context()
def request(method, path, body=None):
    req = urllib.request.Request(base + path, method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, context=context, timeout=30) as response:
        return json.load(response)
request('POST', '/api/v1/namespaces', {'apiVersion':'v1','kind':'Namespace','metadata':{'name':'snapshot-race'}})
path='/api/v1/namespaces/snapshot-race/configmaps'
finished=threading.Event()
def write_batch(worker):
    writes=[]
    for i in range(100):
        name=f'w{worker}-{i}'
        obj=request('POST',path,{'apiVersion':'v1','kind':'ConfigMap','metadata':{'name':name}})
        writes.append((name,int(obj['metadata']['resourceVersion'])))
    return writes
# The server's revisions are numeric store revisions; this probe checks its
# wire contract, not a client's ordering assumptions for opaque Kubernetes RVs.
def read_snapshots(limit):
    snapshots=[]
    while not finished.is_set() or len(snapshots)<50:
        obj=request('GET',path+f'?limit={limit}')
        revision=int(obj['metadata']['resourceVersion'])
        names={item['metadata']['name'] for item in obj['items']}
        continuation=obj['metadata'].get('continue')
        while continuation:
            obj=request('GET',path+'?'+urllib.parse.urlencode({'limit':limit,'continue':continuation}))
            assert int(obj['metadata']['resourceVersion']) == revision, 'pagination changed snapshot revision'
            page_names={item['metadata']['name'] for item in obj['items']}
            assert not names.intersection(page_names), 'pagination duplicated an object'
            names.update(page_names)
            continuation=obj['metadata'].get('continue')
        snapshots.append((revision,names))
    return snapshots
with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
    readers=[pool.submit(read_snapshots,limit) for limit in (1000,37)]
    writers=[pool.submit(write_batch,n) for n in range(4)]
    try:
        writes=[entry for writer in writers for entry in writer.result()]
    finally:
        finished.set()
    snapshots=[entry for reader in readers for entry in reader.result()]
violations=[]
for revision,names in snapshots:
    missing=[(name,rv) for name,rv in writes if rv<=revision and name not in names]
    future=[(name,rv) for name,rv in writes if rv>revision and name in names]
    if missing or future:
        violations.append((revision,missing[:3],future[:3]))
print(f'Checked {len(snapshots)} snapshots against {len(writes)} acknowledged creates',flush=True)
for revision,missing,future in violations[:10]:
    print(f'INCONSISTENT LIST rv={revision}: missing acknowledged writes={missing}; future writes={future}',flush=True)
assert not violations, f'{len(violations)} LIST snapshots disagree with their revision; LIST/WATCH may skip writes'
print('PASS every complete and paginated LIST agrees with its revision',flush=True)
# Replay after several actual snapshots, including early ones. Every write
# omitted from a consistent snapshot must arrive on the following WATCH.
checked=0
for revision,names in sorted(snapshots, key=lambda entry: entry[0]):
    expected={name:rv for name,rv in writes if rv>revision}
    if not expected:
        continue
    query=urllib.parse.urlencode({'watch':'true','resourceVersion':revision,'timeoutSeconds':5})
    req=urllib.request.Request(base+path+'?'+query,
        headers={'Authorization':'Bearer '+token,'Accept':'application/json'})
    seen={}
    with urllib.request.urlopen(req,context=context,timeout=10) as response:
        for line in response:
            event=json.loads(line)
            if event['type']=='BOOKMARK':
                continue
            assert event['type']=='ADDED', f'unexpected watch event: {event}'
            obj=event['object']; name=obj['metadata']['name']
            assert name not in names and name not in seen, f'duplicate LIST/WATCH member: {name}'
            seen[name]=int(obj['metadata']['resourceVersion'])
    assert seen==expected, f'WATCH after {revision}: missing={expected.keys()-seen.keys()}, unexpected={seen.keys()-expected.keys()}'
    checked+=1
    if checked==3:
        break
assert checked==3, 'not enough concurrent snapshots to exercise LIST/WATCH handoff'
print(f'PASS {checked} LIST/WATCH handoffs deliver the exact remaining writes',flush=True)
PY
[ "$?" -eq 0 ] || fail 'LIST snapshot/revision consistency'
report
