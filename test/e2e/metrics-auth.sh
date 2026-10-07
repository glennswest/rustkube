#!/usr/bin/env bash
#
# Metrics (#90) on a real control plane on fastetcd.
#
# apiserver /metrics, now inside authentication and RBAC:
#   - no token 401; a user without a grant 403; system:monitoring 200
#   - histograms carry _bucket series (no summaries), upstream's buckets for
#     apiserver_request_duration_seconds; process_cpu_seconds_total typed counter
# controller-manager :10257 and scheduler :10259, with --tls-cert-file:
#   - HTTPS only (a plain-HTTP request gets nothing)
#   - /healthz, /livez open; /metrics: no token 401, a token the apiserver
#     does not accept 401, no grant 403, system:monitoring 200
#   - controller_reconcile_duration_seconds_bucket exists once controllers
#     have run;
#     leader_election_master_status is 1
#
# Exit status is the number of failed checks.
mkdir -p "$PWD/tmp"
MT=$(mktemp -d "$PWD/tmp/metrics-auth.XXXXXX")
# lib.sh makes the CA, so the components' serving pair is made after it but
# before they start: RK_CM_ARGS / RK_SCHED_ARGS name files filled in below.
RK_CM_ARGS="--tls-cert-file $MT/serving.crt --tls-private-key-file $MT/serving.key"
RK_SCHED_ARGS="$RK_CM_ARGS"
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
trap 'cleanup; rm -rf "$MT"' EXIT

openssl req -newkey rsa:2048 -nodes -keyout "$MT/serving.key" -out "$MT/serving.csr" -subj /CN=localhost 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\n' >"$MT/serving.ext"
openssl x509 -req -in "$MT/serving.csr" -CA "$W/ca.crt" -CAkey "$W/ca.key" -CAcreateserial \
  -out "$MT/serving.crt" -days 2 -extfile "$MT/serving.ext" 2>/dev/null
start_controller_manager
start_scheduler

BOB=$(token bob '[]')
MON=$(token prometheus '["system:monitoring"]')
code() { # <url> [token] — HTTP code
  curl -sk -o "$W/out" -w '%{http_code}' ${2:+-H "Authorization: Bearer $2"} "$1"
}
expect() { # <what> <want> <url> [token]
  local got; got=$(code "$3" "${4:-}")
  [ "$got" = "$2" ] && pass "$1 ($got)" || fail "$1: want $2, got $got $(head -c 200 "$W/out")"
}

# --- apiserver ---------------------------------------------------------------
expect "apiserver /metrics without a token" 401 "$API/metrics"
expect "apiserver /metrics, no grant" 403 "$API/metrics" "$BOB"
expect "apiserver /metrics as system:monitoring" 200 "$API/metrics" "$MON"
grep -q '^apiserver_request_duration_seconds_bucket{.*le="0.025"' "$W/out" \
  && pass "apiserver_request_duration_seconds has upstream's buckets" || fail "no apiserver_request_duration_seconds buckets"
grep -q 'quantile=' "$W/out" && fail "a histogram is still rendered as a summary" || pass "no summaries"
grep -q '^# TYPE process_cpu_seconds_total counter' "$W/out" \
  && pass "process_cpu_seconds_total is a counter" || fail "process_cpu_seconds_total TYPE: $(grep 'TYPE process_cpu' "$W/out")"

# --- controller-manager and scheduler -----------------------------------------
for i in $(seq 60); do code https://127.0.0.1:10257/healthz >/dev/null; [ "$(cat "$W/out")" = ok ] && break; sleep 0.5; done
# Something for the controllers to reconcile.
curl -sk -o /dev/null -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' \
  -d '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"metrics-auth"}}' "$API/api/v1/namespaces"
sleep 3
for comp in "controller-manager 10257" "scheduler 10259"; do
  set -- $comp; name=$1; port=$2
  c=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://127.0.0.1:$port/healthz")
  [ "$c" != 200 ] && pass "$name :$port does not answer plain HTTP ($c)" || fail "$name :$port answered plain HTTP"
  expect "$name /healthz is open" 200 "https://127.0.0.1:$port/healthz"
  expect "$name /livez is open" 200 "https://127.0.0.1:$port/livez"
  expect "$name /metrics without a token" 401 "https://127.0.0.1:$port/metrics"
  expect "$name /metrics with a token nobody issued" 401 "https://127.0.0.1:$port/metrics" "not-a-token"
  expect "$name /metrics, no grant" 403 "https://127.0.0.1:$port/metrics" "$BOB"
  expect "$name /metrics as system:monitoring" 200 "https://127.0.0.1:$port/metrics" "$MON"
  grep -q '^leader_election_master_status{.*} 1' "$W/out" && pass "$name leader_election_master_status 1" \
    || fail "$name leader gauge: $(grep leader_election "$W/out")"
  if [ "$name" = controller-manager ]; then
    grep -q '^controller_reconcile_duration_seconds_bucket{' "$W/out" \
      && pass "controller_reconcile_duration_seconds is emitted, with buckets" || fail "no controller_reconcile_duration_seconds_bucket"
  fi
done
report
