#!/usr/bin/env bash
# The reflector's own WATCH deadline is a routine end, not an outage (#207).
#
# The reflector asks for timeoutSeconds=300 and ends a watch itself at 330 s
# if the server has not (the apiserver ignores timeoutSeconds, #165). Before
# #207 that end was handled as a transport failure: a "reflector WATCH
# reconnecting" warning, every informer briefly unsynchronized (the GC fails
# closed), a reconnect counted and backed off, and a recovery wake on resume.
#
# An idle controller-manager and scheduler run for 400 s, past the deadline:
#   - their watches did end and resume (the apiserver saw new WATCHes),
#   - neither logged "reflector WATCH reconnecting",
#   - neither re-LISTed anything (a resume continues from its revision),
#   - the controller-manager's reconnect counter did not move, when its fixed
#     metrics port is the rig's own.
. "$(dirname "$0")/lib.sh"
start_controller_manager
start_scheduler

# The apiserver counts a GET of /metrics itself as verb="list",
# resource="metrics": the rig's own scrapes are not counted.
counter() { # <verb> — apiserver requests of that verb, 404s excluded
  curl --max-time 5 -sk -H "Authorization: Bearer $ADMIN" "$API/metrics" | grep '^apiserver_request_total{' |
    grep "verb=\"$1\"" | grep -v 'code="404"' | grep -v 'resource="metrics"' |
    awk '{ n += $2 } END { printf "%d\n", n }'
}
reconnects() { # controller-manager's rustkube_watch_reconnects_total, or empty
  curl --max-time 5 -s -H "Authorization: Bearer $ADMIN" http://127.0.0.1:10257/metrics 2>/dev/null |
    awk '/^rustkube_watch_reconnects_total/ { n += $2; seen = 1 } END { if (seen) printf "%d\n", n; else if (NR) print 0 }'
}
warnings() { # <log> — reconnect warnings so far
  sed 's/\x1b\[[0-9;]*m//g' "$1" | grep -c 'reflector WATCH reconnecting'
}

# Let both open their watches and settle. Startup may legitimately retry.
sleep 20
series() { curl --max-time 5 -sk -H "Authorization: Bearer $ADMIN" "$API/metrics" | grep '^apiserver_request_total{' | grep 'verb="list"'; }
series >"$W/lists.before"
watch0=$(counter watch); list0=$(counter list)
cm0=$(warnings "$W/cm.log"); sched0=$(warnings "$W/sched.log")
rc0=$(reconnects)
echo "rig: idle from $(date -u +%H:%M:%S): $watch0 watches, $list0 lists, reconnect counter '${rc0}'"
sleep 400   # longer than the 330 s client deadline
watch1=$(counter watch); list1=$(counter list)
series >"$W/lists.after"
cm1=$(warnings "$W/cm.log"); sched1=$(warnings "$W/sched.log")
rc1=$(reconnects)
echo "rig: idle to $(date -u +%H:%M:%S): $watch1 watches, $list1 lists, reconnect counter '${rc1}'"

if [ "$watch1" -gt "$watch0" ]; then pass "watches ended and resumed: $((watch1 - watch0)) new WATCHes in 400 s"
else fail "no watch ended in 400 s: the deadline was not exercised"; fi
if [ "$cm1" -eq "$cm0" ]; then pass "controller-manager: no reflector WATCH reconnecting"
else fail "controller-manager logged reflector WATCH reconnecting $((cm1 - cm0)) times"
  sed 's/\x1b\[[0-9;]*m//g' "$W/cm.log" | grep 'reflector WATCH reconnecting' | tail -5; fi
if [ "$sched1" -eq "$sched0" ]; then pass "scheduler: no reflector WATCH reconnecting"
else fail "scheduler logged reflector WATCH reconnecting $((sched1 - sched0)) times"
  sed 's/\x1b\[[0-9;]*m//g' "$W/sched.log" | grep 'reflector WATCH reconnecting' | tail -5; fi
# Nothing relisted (KubeVirt's unserved 404 retries aside, #172).
if [ "$list1" -eq "$list0" ]; then pass "no LIST while idle (watches resumed from their revisions)"
else fail "$((list1 - list0)) LISTs while idle"
  awk 'NR==FNR { b[$1]=$2; next } { d=$2-b[$1]; if (d>0) print "  +" d, $1 }' "$W/lists.before" "$W/lists.after"
  sed 's/\x1b\[[0-9;]*m//g' "$W/cm.log" "$W/sched.log" | grep -E ' WARN ' | grep -v '404 Not Found' | tail -20; fi
# 10257 is fixed; another rig on the box may own it, so this check only
# counts when the port answered both times.
if [ -n "$rc0" ] && [ -n "$rc1" ]; then
  if [ "$rc1" -eq "$rc0" ]; then pass "controller-manager reconnect counter unchanged ($rc1)"
  else fail "controller-manager counted $((rc1 - rc0)) watch reconnects"; fi
else
  echo "rig: controller-manager metrics port not reachable; reconnect counter not checked"
fi
report
