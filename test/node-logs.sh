#!/usr/bin/env bash
#
# `/test node-logs` (#180): reads the node's pod logs (the host's
# /var/log/pods, mounted read-only at /host/var/log/pods by
# test/requires.toml) for the errors client-go logs when a watch stream from
# this apiserver cannot be read. #180's report came from server1's cilium
# agent ("unable to decode an event from the watch stream" on its
# metadata-only CRD watch); this asks every machine the same question, with
# no ssh and no cluster credentials.
#
# One JSON line per check and a summary, as the other suites. Exit 0 passed,
# 1 a check failed, 2 nothing to read.
set -u
root=${STORM_HOST_ROOT:-/host}/var/log/pods
# client-go's reflector/watch messages for a stream it could not read
# (k8s.io/client-go/tools/cache/reflector.go, watch/streamwatcher.go).
pattern='unable to decode an event from the watch stream|unable to decode watch event|watch of .* ended with: an error on the server'

json() { # <test> <status> <ms> <detail>
  python3 -I -c 'import json,sys; print(json.dumps({"test": sys.argv[1], "status": sys.argv[2], "ms": int(sys.argv[3]), "detail": sys.argv[4]}), flush=True)' "$@"
}

if ! [ -d "$root" ] || [ -z "$(ls -A "$root" 2>/dev/null)" ]; then
  json pod-logs fail 0 "no pod logs at $root (is host_paths_read_only /var/log/pods declared and mounted?)"
  echo '{"summary": {"pass": 0, "fail": 1, "skip": 0}}'; exit 2
fi

pass=0 fail=0 skip=0
t0=$(date +%s%3N)
files=$(find "$root" -type f -name '*.log' 2>/dev/null)
nfiles=$(printf '%s\n' "$files" | grep -c .)
bytes=$(printf '%s\n' "$files" | xargs -r cat 2>/dev/null | wc -c)
oldest=$(printf '%s\n' "$files" | xargs -r head -qn1 2>/dev/null | grep -oE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}' | sort | head -1)
json pod-logs pass $(( $(date +%s%3N) - t0 )) "$nfiles log files, $bytes bytes, oldest line ${oldest:-unknown}"
pass=$((pass + 1))

# The cilium agent: #180's client. Absent on a flowsdn machine: a skip.
agent=$(printf '%s\n' "$files" | grep -E '/kube-system_cilium-[a-z0-9]{5}_[^/]+/cilium-agent/' || true)
if [ -n "$agent" ]; then
  json cilium-agent-logs pass 0 "$(printf '%s\n' "$agent" | grep -c .) file(s): $(printf '%s\n' "$agent" | sed "s#^$root/##" | tr '\n' ' ')"
  pass=$((pass + 1))
else
  json cilium-agent-logs skip 0 "no kube-system cilium agent on this node"
  skip=$((skip + 1))
fi

# Every pod's log: any client-go on the node may hit it, not only cilium.
t0=$(date +%s%3N)
hits=$(printf '%s\n' "$files" | xargs -r grep -HaE "$pattern" 2>/dev/null || true)
ms=$(( $(date +%s%3N) - t0 ))
if [ -z "$hits" ]; then
  json watch-decode-errors pass "$ms" "no watch-stream decode errors in any pod log ($pattern)"
  pass=$((pass + 1))
else
  n=$(printf '%s\n' "$hits" | grep -c .)
  by=$(printf '%s\n' "$hits" | sed "s#^$root/##; s#/.*##" | sort | uniq -c | sort -rn | head -5 | awk '{printf "%s×%s ", $2, $1}')
  json watch-decode-errors fail "$ms" "$n line(s) in: $by"
  fail=$((fail + 1))
  # The whole text is what #180 needs: the frame or the stream error.
  { echo "---- watch-decode-errors (first 20, last 20)"
    printf '%s\n' "$hits" | sed "s#^$root/##" | head -20
    echo "…"
    printf '%s\n' "$hits" | sed "s#^$root/##" | tail -20; } >&2
fi

echo "{\"summary\": {\"pass\": $pass, \"fail\": $fail, \"skip\": $skip}}"
[ "$fail" = 0 ] && exit 0 || exit 1
