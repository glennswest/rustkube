#!/usr/bin/env bash
# Object-worker safety on a disposable API/store rig, without a kubelet.
. "$(dirname "$0")/lib.sh"
start_controller_manager
export API ADMIN
python3 - <<'PY'
import json, os, ssl, time, urllib.request, urllib.error
base, token = os.environ['API'], os.environ['ADMIN']; context=ssl._create_unverified_context()
def req(method,path,body=None):
    request=urllib.request.Request(base+path,method=method,data=None if body is None else json.dumps(body).encode(),headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
    with urllib.request.urlopen(request,context=context,timeout=10) as r:return json.load(r)
def get(path):
    try:return req('GET',path)
    except urllib.error.HTTPError as e:
        if e.code==404:return None
        raise
count=0
def wait(label,test):
    global count
    end=time.monotonic()+25
    while time.monotonic()<end:
        if test():print('PASS',label,flush=True);count+=1;return
        time.sleep(.1)
    raise AssertionError(label)
req('POST','/api/v1/namespaces',{'metadata':{'name':'indexed-safe'}})
ns='/api/v1/namespaces/indexed-safe'
# Foreground, orphan and background propagation use observed UID/RV guards.
for policy in ['Foreground','Orphan','Background']:
    name=policy.lower()
    owner=req('POST',ns+'/configmaps',{'apiVersion':'v1','kind':'ConfigMap','metadata':{'name':name}})
    child=req('POST',ns+'/secrets',{'apiVersion':'v1','kind':'Secret','metadata':{'name':name,'ownerReferences':[{'apiVersion':'v1','kind':'ConfigMap','name':name,'uid':owner['metadata']['uid'],'blockOwnerDeletion':True}]}})
    req('DELETE',ns+'/configmaps/'+name,{'propagationPolicy':policy,'preconditions':{'uid':owner['metadata']['uid']}})
    wait(policy+' owner finishes',lambda:get(ns+'/configmaps/'+name) is None)
    if policy=='Orphan':
        wait('orphan survives without reference',lambda:(get(ns+'/secrets/'+name) or {}).get('metadata',{}).get('ownerReferences')==[])
    else:wait(policy+' collects dependent',lambda:get(ns+'/secrets/'+name) is None)
# Retention is an object deadline, not a periodic collection pass.
req('POST',ns+'/events',{'apiVersion':'v1','kind':'Event','metadata':{'name':'expired'},'lastTimestamp':'2000-01-01T00:00:00Z','involvedObject':{'kind':'Pod','namespace':'indexed-safe','name':'none','uid':'none'},'reason':'Test','message':'expired'})
wait('expired Event collected',lambda:get(ns+'/events/expired') is None)
# Namespace teardown uses discovered feeds, including a finalizer-held child.
req('POST','/api/v1/namespaces',{'metadata':{'name':'indexed-teardown'}})
t='/api/v1/namespaces/indexed-teardown'
req('POST',t+'/configmaps',{'apiVersion':'v1','kind':'ConfigMap','metadata':{'name':'held','finalizers':['example.test/hold']}})
req('DELETE','/api/v1/namespaces/indexed-teardown',{})
wait('namespace deletion reaches held child',lambda:bool((get(t+'/configmaps/held') or {}).get('metadata',{}).get('deletionTimestamp')))
assert get('/api/v1/namespaces/indexed-teardown') is not None
held=get(t+'/configmaps/held');held['metadata']['finalizers']=[];req('PUT',t+'/configmaps/held',held)
wait('namespace finalizes after child release',lambda:get('/api/v1/namespaces/indexed-teardown') is None)
# Set up an over-capacity burst BEFORE starting the scheduler.
node=req('POST','/api/v1/nodes',{'apiVersion':'v1','kind':'Node','metadata':{'name':'capacity-one'}})
node['status']={'capacity':{'cpu':'1','memory':'4Gi','pods':'100'},'allocatable':{'cpu':'1','memory':'4Gi','pods':'100'},'conditions':[{'type':'Ready','status':'True'}]}
req('PUT','/api/v1/nodes/capacity-one/status',node)
for i in range(6):
    req('POST',ns+'/pods',{'apiVersion':'v1','kind':'Pod','metadata':{'name':'burst-'+str(i)},'spec':{'containers':[{'name':'c','image':'unused','resources':{'requests':{'cpu':'600m','memory':'64Mi'}}}]}})
print(f'{count} controller safety checks passed',flush=True)
PY
[ "$?" -eq 0 ] || { fail 'controller safety'; report; }
start_scheduler
python3 - <<'PY'
import json, os, ssl, time, urllib.request
base=os.environ['API'];context=ssl._create_unverified_context()
def req(method,path,body=None):
    r=urllib.request.Request(base+path,method=method,data=None if body is None else json.dumps(body).encode(),headers={'Authorization':'Bearer '+os.environ['ADMIN'],'Content-Type':'application/json'})
    with urllib.request.urlopen(r,context=context,timeout=10) as s:return json.load(s)
path='/api/v1/namespaces/indexed-safe/pods'
def bound():return [p for p in req('GET',path)['items'] if p['spec'].get('nodeName') and p.get('status',{}).get('phase') not in ['Succeeded','Failed']]
end=time.monotonic()+20
while not bound() and time.monotonic()<end:time.sleep(.1)
assert len(bound())==1,'burst overcommitted one CPU'
time.sleep(2);assert len(bound())==1,'later work overcommitted one CPU'
print('PASS six 600m Pods bind only one onto a one-CPU Node',flush=True)
first=bound()[0];first.setdefault('status',{})['phase']='Succeeded'
req('PUT',path+'/'+first['metadata']['name']+'/status',first)
end=time.monotonic()+20
while not bound() and time.monotonic()<end:time.sleep(.1)
assert len(bound())==1 and bound()[0]['metadata']['uid']!=first['metadata']['uid'],'capacity release failed to wake pending Pods'
time.sleep(2);assert len(bound())==1,'released CPU was spent twice'
print('PASS terminal Pod releases capacity exactly once and wakes pending work',flush=True)
PY
[ "$?" -eq 0 ] || fail 'scheduler reservation safety'
report
