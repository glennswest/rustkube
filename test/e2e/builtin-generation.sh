#!/usr/bin/env bash
#
# metadata.generation for built-in objects (#285), as upstream's strategies:
#
# - a Deployment is created at generation 1, whatever the client sent;
#   a spec change (scale by PATCH) moves it to 2; a label leaves it; a
#   /status write leaves it; the controller-manager reports
#   status.observedGeneration 2 (kubectl rollout status compares the two)
# - the controller's revision annotation (on its /status write) leaves the
#   Deployment's generation; its ReplicaSet reports status.observedGeneration
#   equal to its own generation
# - a Pod starts at 1, the scheduler's bind (a PUT of spec.nodeName, to a
#   Ready Node the rig makes) leaves it, an image change moves it to 2
# - a ConfigMap has none, and a client cannot give it one
# - a DELETE that leaves the object on a finalizer moves it on
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

start_controller_manager
start_scheduler

get() { curl -sk -H "Authorization: Bearer $ADMIN" "$API$1"; }
send() { # <method> <path> <content-type> <body>
  curl -sk -o /dev/null -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" -H "Content-Type: $3" "$API$2" -d "$4"
}
field() { python3 -c 'import json,sys
o=json.load(sys.stdin)
for k in sys.argv[1].split("."): o=(o or {}).get(k) if isinstance(o, dict) else None
print(o)' "$1"; }
check() { # <what> <want> <got>
  [ "$3" = "$2" ] && pass "$1: $3" || fail "$1: want $2, got $3"
}

D=/apis/apps/v1/namespaces/default/deployments
send POST $D application/json '{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"gen","generation":7},"spec":{"replicas":0,"selector":{"matchLabels":{"app":"gen"}},"template":{"metadata":{"labels":{"app":"gen"}},"spec":{"containers":[{"name":"c","image":"pause"}]}}}}' >/dev/null
check "Deployment created" 1 "$(get $D/gen | field metadata.generation)"
send PATCH $D/gen application/merge-patch+json '{"spec":{"replicas":1}}' >/dev/null
check "Deployment scaled (spec)" 2 "$(get $D/gen | field metadata.generation)"
send PATCH $D/gen application/merge-patch+json '{"metadata":{"labels":{"x":"y"}}}' >/dev/null
check "Deployment labelled" 2 "$(get $D/gen | field metadata.generation)"
send PATCH $D/gen/status application/merge-patch+json '{"status":{"collisionCount":0}}' >/dev/null
check "Deployment /status" 2 "$(get $D/gen | field metadata.generation)"
for _ in $(seq 30); do
  og=$(get $D/gen | field status.observedGeneration)
  [ "$og" = 2 ] && break
  sleep 1
done
check "Deployment status.observedGeneration" 2 "$og"
check "Deployment after the controller's revision annotation" 2 "$(get $D/gen | field metadata.generation)"
rsgen() { get "/apis/apps/v1/namespaces/default/replicasets?labelSelector=app%3Dgen" | python3 -c 'import json,sys
i=json.load(sys.stdin)["items"]
print("no ReplicaSet" if not i else "%s/%s" % (i[0]["metadata"].get("generation"), i[0].get("status", {}).get("observedGeneration")))'; }
for _ in $(seq 30); do
  rs=$(rsgen); g=${rs%/*}
  [ "$rs" = "$g/$g" ] && break
  sleep 1
done
[ "$rs" = "$g/$g" ] && [ "$g" != None ] && pass "ReplicaSet observedGeneration = generation ($rs)" || fail "ReplicaSet generation/observedGeneration: $rs"

# A Ready Node for the scheduler to bind to.
curl -sk -o /dev/null -X POST -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' "$API/api/v1/nodes" \
  -d '{"apiVersion":"v1","kind":"Node","metadata":{"name":"gen-node","labels":{"kubernetes.io/hostname":"gen-node"}}}'
get /api/v1/nodes/gen-node | python3 -c 'import json,sys
n=json.load(sys.stdin); r={"cpu":"8","memory":"16Gi","pods":"110"}
n["status"]={"capacity":r,"allocatable":r,"conditions":[{"type":"Ready","status":"True"}]}
print(json.dumps(n))' > "$W/node.json"
curl -sk -o /dev/null -X PUT -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' "$API/api/v1/nodes/gen-node/status" -d @"$W/node.json"

P=/api/v1/namespaces/default/pods
send POST $P application/json '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"genpod"},"spec":{"containers":[{"name":"c","image":"pause"}]}}' >/dev/null
check "Pod created" 1 "$(get $P/genpod | field metadata.generation)"
for _ in $(seq 30); do
  node=$(get $P/genpod | field spec.nodeName)
  [ "$node" != None ] && break
  sleep 1
done
[ "$node" != None ] && pass "Pod bound to $node" || fail "Pod never bound"
check "Pod after the bind" 1 "$(get $P/genpod | field metadata.generation)"
send PATCH $P/genpod application/strategic-merge-patch+json '{"spec":{"containers":[{"name":"c","image":"pause:2"}]}}' >/dev/null
check "Pod image changed" 2 "$(get $P/genpod | field metadata.generation)"

C=/api/v1/namespaces/default/configmaps
send POST $C application/json '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"gencm"},"data":{"a":"1"}}' >/dev/null
check "ConfigMap created" None "$(get $C/gencm | field metadata.generation)"
send PATCH $C/gencm application/merge-patch+json '{"metadata":{"generation":5},"data":{"a":"2"}}' >/dev/null
check "ConfigMap: a client's generation" None "$(get $C/gencm | field metadata.generation)"

send PATCH $D/gen application/merge-patch+json '{"metadata":{"finalizers":["example.com/hold"]}}' >/dev/null
send DELETE $D/gen application/json '{}' >/dev/null
check "Deployment deletion started" 3 "$(get $D/gen | field metadata.generation)"
send PATCH $D/gen application/merge-patch+json '{"metadata":{"finalizers":null}}' >/dev/null
report
