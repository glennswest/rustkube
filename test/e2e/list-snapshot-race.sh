#!/usr/bin/env bash
# LIST contents and resourceVersion must describe the same snapshot. No controllers.
. "$(dirname "$0")/lib.sh"
export API ADMIN
python3 - <<'PY'
import concurrent.futures, json, os, ssl, threading, time, urllib.request
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
def read_snapshots():
    snapshots=[]
    while not finished.is_set() or len(snapshots)<50:
        obj=request('GET',path+'?limit=1000')
        snapshots.append((int(obj['metadata']['resourceVersion']),
            {item['metadata']['name'] for item in obj['items']}))
    return snapshots
with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
    readers=[pool.submit(read_snapshots) for _ in range(2)]
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
print('PASS every LIST agrees with its revision',flush=True)
PY
[ "$?" -eq 0 ] || fail 'LIST snapshot/revision consistency'
report
