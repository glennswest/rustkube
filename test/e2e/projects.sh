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

# --- escalation prevention (#98) -----------------------------------------------
# A project admin shares what she holds, and nothing more.
expect_denied "alice: bind cluster-admin in demo" as "$ALICE" alice create rolebinding ca -n demo --clusterrole=cluster-admin --user=alice
expect_denied "alice: repoint her admin binding at cluster-admin" as "$ALICE" alice patch rolebinding admin -n demo --type=merge -p '{"roleRef":{"name":"cluster-admin"}}'
expect_denied "alice: a Role granting everything" as "$ALICE" alice apply -f - <<'YAML'
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata: {name: god, namespace: demo}
rules: [{apiGroups: ["*"], resources: ["*"], verbs: ["*"]}]
YAML
expect_denied "alice: a Role granting update namespaces" as "$ALICE" alice create role ns-writer -n demo --verb=update --resource=namespaces
expect_ok "alice: a Role within her rights" as "$ALICE" alice create role cm-reader -n demo --verb=get,list --resource=configmaps
expect_ok "alice: bind that Role to bob" as "$ALICE" alice create rolebinding cm-reader -n demo --role=cm-reader --user=bob
expect_ok "alice: add-role-to-user edit bob -n demo" as "$ALICE" alice adm policy add-role-to-user edit bob -n demo
expect_ok "alice: edit her own binding's labels" as "$ALICE" alice label rolebinding admin -n demo reviewed=yes

# Cluster scope: carol may write ClusterRoleBindings, and is no cluster-admin.
CAROL=$(token carol '[]')
as "$ADMIN" admin apply -f - >/dev/null <<'YAML'
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata: {name: crb-writer}
rules:
- {apiGroups: ["rbac.authorization.k8s.io"], resources: ["clusterrolebindings", "clusterroles"], verbs: ["create", "get", "list"]}
- {apiGroups: ["rbac.authorization.k8s.io"], resources: ["clusterroles"], verbs: ["bind"], resourceNames: ["view"]}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata: {name: carol-crb-writer}
roleRef: {apiGroup: rbac.authorization.k8s.io, kind: ClusterRole, name: crb-writer}
subjects: [{kind: User, name: carol, apiGroup: rbac.authorization.k8s.io}]
YAML
expect_denied "carol: bind herself cluster-admin" as "$CAROL" carol create clusterrolebinding carol-root --clusterrole=cluster-admin --user=carol
expect_denied "carol: bind edit (neither held nor bind)" as "$CAROL" carol create clusterrolebinding carol-edit --clusterrole=edit --user=carol
expect_ok "carol: bind view (holds bind on it)" as "$CAROL" carol create clusterrolebinding carol-view --clusterrole=view --user=carol
expect_ok "carol: bind crb-writer (holds every rule)" as "$CAROL" carol create clusterrolebinding carol-again --clusterrole=crb-writer --user=bob
expect_denied "carol: a ClusterRole beyond her rights" as "$CAROL" carol create clusterrole wide --verb='*' --resource=secrets
as "$ADMIN" admin create clusterrole escalator --verb=escalate --resource=clusterroles.rbac.authorization.k8s.io >/dev/null
as "$ADMIN" admin create clusterrolebinding carol-escalator --clusterrole=escalator --user=carol >/dev/null
expect_ok "carol: the same ClusterRole once she holds escalate" as "$CAROL" carol create clusterrole wide --verb='*' --resource=secrets
expect_ok "admin (system:masters): binds anything" as "$ADMIN" admin create clusterrolebinding bob-root --clusterrole=cluster-admin --user=bob
as "$ADMIN" admin delete clusterrolebinding bob-root >/dev/null

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
