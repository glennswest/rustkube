#!/usr/bin/env bash
#
# A Job's spec.managedBy (#285, GA in 1.35), with the controller-manager
# running:
#
# - a Job managed by another controller gets no Pods and no status from ours
#   (10 s); a Job without managedBy, or naming kubernetes.io/job-controller,
#   gets its Pod
# - a managedBy that is not a domain-prefixed path, or is over 63 bytes: 422
# - changing it on an update: 422 (immutable)
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

start_controller_manager

J=/apis/batch/v1/namespaces/default/jobs
job() { # <name> <managedBy json or empty>
  local m=""
  [ -n "$2" ] && m="\"managedBy\":$2,"
  printf '{"apiVersion":"batch/v1","kind":"Job","metadata":{"name":"%s"},"spec":{%s"template":{"spec":{"restartPolicy":"Never","containers":[{"name":"c","image":"pause"}]}}}}' "$1" "$m"
}
send() { # <method> <path> <content-type> <body> — HTTP code, body in $W/out
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" -H "Content-Type: $3" "$API$2" -d "$4"
}
pods() { # <job> — how many Pods it has
  curl -sk -H "Authorization: Bearer $ADMIN" "$API/api/v1/namespaces/default/pods?labelSelector=job-name%3D$1" \
    | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["items"]))'
}

got=$(send POST $J application/json "$(job elsewhere '"kueue.x-k8s.io/multikueue"')")
[ "$got" = 201 ] && pass "Job managed elsewhere created" || fail "create managed elsewhere: $got $(head -c 300 "$W/out")"
got=$(send POST $J application/json "$(job ours '')")
[ "$got" = 201 ] && pass "Job without managedBy created" || fail "create ours: $got"
got=$(send POST $J application/json "$(job named '"kubernetes.io/job-controller"')")
[ "$got" = 201 ] && pass "Job naming the job controller created" || fail "create named: $got"

for _ in $(seq 20); do
  [ "$(pods ours)" -ge 1 ] && [ "$(pods named)" -ge 1 ] && break
  sleep 1
done
[ "$(pods ours)" -ge 1 ] && pass "Job without managedBy has its Pod" || fail "Job without managedBy: no Pod"
[ "$(pods named)" -ge 1 ] && pass "Job naming the job controller has its Pod" || fail "Job naming the job controller: no Pod"
sleep 10
n=$(pods elsewhere)
[ "$n" = 0 ] && pass "Job managed elsewhere: no Pods after 10 s" || fail "Job managed elsewhere: $n Pod(s)"
st=$(curl -sk -H "Authorization: Bearer $ADMIN" "$API$J/elsewhere" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("status") or {})')
[ "$st" = "{}" ] && pass "Job managed elsewhere: no status written" || fail "Job managed elsewhere: status $st"

got=$(send POST $J application/json "$(job bad '"no-slash"')")
grep -q 'must be a domain-prefixed path' "$W/out"; ok=$?
[ "$got" = 422 ] && [ $ok = 0 ] && pass "managedBy without a domain: 422" || fail "managedBy no-slash: $got $(head -c 300 "$W/out")"
long="\"acme.io/$(printf 'x%.0s' $(seq 60))\""
got=$(send POST $J application/json "$(job long "$long")")
[ "$got" = 422 ] && pass "managedBy over 63 bytes: 422" || fail "managedBy too long: $got"
got=$(send PATCH $J/elsewhere application/merge-patch+json '{"spec":{"managedBy":"acme.io/other"}}')
grep -q 'field is immutable' "$W/out"; ok=$?
[ "$got" = 422 ] && [ $ok = 0 ] && pass "managedBy changed: 422 immutable" || fail "managedBy change: $got $(head -c 300 "$W/out")"
report
