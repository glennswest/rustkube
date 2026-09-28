#!/usr/bin/env bash
#
# Run the test container's binary (#96) against a real apiserver,
# controller-manager and scheduler on a real fastetcd, the way stormcentral's
# runner would: a namespace of its own, the `storm-test` ServiceAccount with a
# Role of `*` in that namespace only, and that ServiceAccount's token.
#
#   test/e2e/test-container.sh [suite ...]     # default: short
#
# There is no kubelet here: pods are created and bound, never run. The
# checks that need a running pod report fail or skip, and say so; the ones
# this rig can answer must pass. RK_TEST_EXPECT_FAIL is a regex of test names
# allowed to fail here (default: the pod-running ones).
# Exit status is the number of suites that did not end as expected.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

cargo build -q -p rustkube-test || exit 100
RT=$CARGO_TARGET_DIR/debug/rustkube-test
start_controller_manager
start_scheduler

k() { curl -sk -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' "$@"; }
# Two stand-in nodes so the scheduler has somewhere to bind to.
for n in node-a node-b; do
  k -X POST "$API/api/v1/nodes" -d "{\"apiVersion\":\"v1\",\"kind\":\"Node\",\"metadata\":{\"name\":\"$n\",\"labels\":{\"kubernetes.io/hostname\":\"$n\",\"kubernetes.io/os\":\"linux\",\"kubernetes.io/arch\":\"amd64\"}}}" >/dev/null
  k -X PATCH -H 'Content-Type: application/merge-patch+json' "$API/api/v1/nodes/$n/status" -d "{\"status\":{
    \"capacity\":{\"cpu\":\"8\",\"memory\":\"16Gi\",\"pods\":\"110\"},\"allocatable\":{\"cpu\":\"8\",\"memory\":\"16Gi\",\"pods\":\"110\"},
    \"conditions\":[{\"type\":\"Ready\",\"status\":\"True\",\"reason\":\"StandIn\"}]}}" >/dev/null
done

EXPECT=${RK_TEST_EXPECT_FAIL:-'running|endpointslice|job-completes|daemonset|rollout'}
BAD=0
for suite in "${@:-short}"; do
  ns=test-rustkube-$suite-e2e
  k -X POST "$API/api/v1/namespaces" -d "{\"apiVersion\":\"v1\",\"kind\":\"Namespace\",\"metadata\":{\"name\":\"$ns\"}}" >/dev/null
  k -X POST "$API/api/v1/namespaces/$ns/serviceaccounts" -d '{"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"storm-test"}}' >/dev/null
  k -X POST "$API/apis/rbac.authorization.k8s.io/v1/namespaces/$ns/roles" -d '{"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":"storm-test"},"rules":[{"apiGroups":["*"],"resources":["*"],"verbs":["*"]}]}' >/dev/null
  k -X POST "$API/apis/rbac.authorization.k8s.io/v1/namespaces/$ns/rolebindings" -d "{\"apiVersion\":\"rbac.authorization.k8s.io/v1\",\"kind\":\"RoleBinding\",\"metadata\":{\"name\":\"storm-test\"},\"roleRef\":{\"apiGroup\":\"rbac.authorization.k8s.io\",\"kind\":\"Role\",\"name\":\"storm-test\"},\"subjects\":[{\"kind\":\"ServiceAccount\",\"name\":\"storm-test\",\"namespace\":\"$ns\"}]}" >/dev/null
  sa=$W/sa-$suite; mkdir -p "$sa"
  token "system:serviceaccount:$ns:storm-test" '[]' >"$sa/token"
  cp "$W/ca.crt" "$sa/ca.crt"; printf %s "$ns" >"$sa/namespace"

  echo "==== $suite"
  STORM_SA_DIR=$sa STORM_API=$API STORM_NAMESPACE=$ns STORM_RUN_ID=e2e-$suite \
    RUSTKUBE_TEST_IMAGE=rustkube-test:e2e "$RT" "$suite" | tee "$W/$suite.out"
  echo "exit ${PIPESTATUS[0]}"
  # Every fail must be one this rig cannot answer; nothing may be could-not-run.
  if grep -q '"test":"could-not-run"' "$W/$suite.out"; then
    fail "$suite: could not run"
  fi
  unexpected=$(grep '"status":"fail"' "$W/$suite.out" | grep -Ev "\"test\":\"[^\"]*($EXPECT)[^\"]*\"" || true)
  if [ -n "$unexpected" ]; then fail "$suite: unexpected failures: $unexpected"; else pass "$suite: every check this rig can answer passed"; fi
done
report
