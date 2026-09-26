#!/usr/bin/env bash
#
# Projects end to end (#97): a real apiserver + controller-manager on a real
# fastetcd, driven by `oc` as two ordinary users and an administrator.
#
#   test/e2e/projects.sh            # from the checkout; builds what it runs
#
# Setup is lib.sh's; this also fetches the `oc` client. Exit status is the
# number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
ALICE=$(token alice '[]')
BOB=$(token bob '[]')

if ! command -v oc >/dev/null; then
  curl -sfL https://mirror.openshift.com/pub/openshift-v4/x86_64/clients/ocp/stable/openshift-client-linux.tar.gz \
    | tar -xz -C "$W" oc || exit 100
  OC=$W/oc
else
  OC=$(command -v oc)
fi
"$OC" version --client | head -1

# One kubeconfig per user, so `oc new-project` switching context for one
# does not switch it for another.
as() { # <token> <kubeconfig-name> oc-args...
  local t=$1 k=$2; shift 2
  # A cache of its own: oc keeps discovery for hours under ~/.kube/cache,
  # keyed by host:port, so an earlier run's apiserver would answer for this one.
  KUBECONFIG=$W/kc-$k "$OC" --cache-dir "$W/cache-$k" --server "$API" \
    --insecure-skip-tls-verify --token "$t" "$@" 2>&1
}
expect_ok() { # <what> <cmd...>
  local what=$1; shift
  if out=$("$@"); then pass "$what"; else fail "$what: $out"; fi
}
expect_denied() { # <what> <cmd...>
  local what=$1; shift
  if out=$("$@"); then fail "$what: allowed: $out"
  elif grep -qiE 'forbidden|not allowed|cannot' <<<"$out"; then pass "$what"
  else fail "$what: failed, but not as forbidden: $out"; fi
}
names() { tr -s ' \n' '\n' | sed 's|^project.project.openshift.io/||' | sort | tr '\n' ' '; }

# --- self-service -------------------------------------------------------------
expect_ok "alice: oc new-project demo" as "$ALICE" alice new-project demo --display-name=Demo --description="alice's"
expect_ok "bob: oc new-project bobs" as "$BOB" bob new-project bobs
expect_denied "alice: a reserved name is refused" as "$ALICE" alice new-project kube-evil

got=$(as "$ADMIN" admin get ns demo -o jsonpath='{.metadata.annotations.openshift\.io/requester}/{.metadata.annotations.openshift\.io/display-name}/{.status.phase}')
[ "$got" = "alice/Demo/Active" ] && pass "namespace demo: requester, display name, Active" || fail "namespace demo annotations: $got"
got=$(as "$ADMIN" admin get rolebinding admin -n demo -o jsonpath='{.roleRef.name}/{.subjects[0].kind}/{.subjects[0].name}')
[ "$got" = "admin/User/alice" ] && pass "rolebinding demo/admin binds alice to admin" || fail "rolebinding demo/admin: $got"

# --- visibility ---------------------------------------------------------------
got=$(as "$ALICE" alice get projects -o name | names)
[ "$got" = "demo " ] && pass "alice sees only demo" || fail "alice sees: $got"
got=$(as "$BOB" bob get projects -o name | names)
[ "$got" = "bobs " ] && pass "bob sees only bobs" || fail "bob sees: $got"
out=$(as "$ALICE" alice projects); echo "$out" | sed 's/^/      /'
grep -q demo <<<"$out" && ! grep -q bobs <<<"$out" && pass "alice: oc projects" || fail "alice: oc projects"
out=$(as "$ALICE" alice get projects); echo "$out" | sed 's/^/      /'
grep -qE '^demo +Demo +Active' <<<"$out" && pass "alice: oc get projects table" || fail "oc get projects table"
expect_ok "alice: oc project demo" as "$ALICE" alice project demo
expect_denied "bob: get project demo" as "$BOB" bob get project demo
expect_denied "bob: get pods -n demo" as "$BOB" bob get pods -n demo
got=$(as "$ADMIN" admin get projects -o name | names)
grep -q "default" <<<"$got" && grep -q "kube-system" <<<"$got" && grep -q "bobs" <<<"$got" \
  && pass "admin sees every project" || fail "admin sees: $got"

# --- working in a project -----------------------------------------------------
expect_ok "alice: create configmap in demo" as "$ALICE" alice create configmap c1 -n demo --from-literal=a=b
expect_ok "alice: create secret in demo" as "$ALICE" alice create secret generic s1 -n demo --from-literal=a=b
expect_ok "alice: create deployment in demo" as "$ALICE" alice create deployment web -n demo --image=registry.invalid/web
expect_ok "alice: oc get all -n demo" as "$ALICE" alice get all -n demo
expect_denied "alice: cannot relabel her namespace" as "$ALICE" alice label ns demo pod-security.kubernetes.io/enforce=privileged
expect_denied "alice: cannot list namespaces" as "$ALICE" alice get ns
expect_denied "alice: cannot read another project" as "$ALICE" alice get cm -n bobs

# --- sharing ------------------------------------------------------------------
expect_ok "alice: add-role-to-user view bob -n demo" as "$ALICE" alice adm policy add-role-to-user view bob -n demo
got=$(as "$BOB" bob get projects -o name | names)
[ "$got" = "bobs demo " ] && pass "bob now sees demo too" || fail "bob sees: $got"
expect_ok "bob: get configmaps -n demo" as "$BOB" bob get cm -n demo
expect_denied "bob (view): read secrets in demo" as "$BOB" bob get secrets -n demo
expect_denied "bob (view): delete in demo" as "$BOB" bob delete cm c1 -n demo
expect_denied "bob (view): delete project demo" as "$BOB" bob delete project demo

# --- deletion cascades --------------------------------------------------------
expect_ok "alice: delete project demo" as "$ALICE" alice delete project demo
gone=
for _ in $(seq 90); do
  if as "$ADMIN" admin get ns demo 2>&1 | grep -qi 'not found'; then gone=1; break; fi
  sleep 1
done
[ -n "$gone" ] && pass "namespace demo removed by the cascade" || fail "namespace demo still present: $(as "$ADMIN" admin get ns demo -o jsonpath='{.status.phase}')"
got=$(as "$ADMIN" admin get cm,secrets,deployments -n demo -o name 2>&1 | grep -v '^$' | grep -vi 'kube-root-ca\|default-token' || true)
[ -z "$got" ] && pass "nothing left in demo" || fail "left in demo: $got"

# --- turning self-service off -------------------------------------------------
expect_ok "admin: empty self-provisioners" as "$ADMIN" admin patch clusterrolebinding.rbac self-provisioners --type=json -p '[{"op":"remove","path":"/subjects"}]'
expect_denied "alice: new-project refused when self-service is off" as "$ALICE" alice new-project demo2

report
