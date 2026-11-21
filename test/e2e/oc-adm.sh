#!/usr/bin/env bash
#
# `oc adm`, verb by verb, against a real apiserver + controller-manager on a
# real fastetcd (#69). There is no kubelet: Nodes are API objects created here,
# and pods are bound to them by `spec.nodeName`, which is all the node verbs
# look at.
#
#   test/e2e/oc-adm.sh              # from the checkout; builds what it runs
#
# Each verb is expected to work (`works`) or expected to fail for a reason
# recorded in docs/oc-compatibility.md (`missing`). A verb that does the other
# thing is a failure of this script — so a gap that closes, or one that opens,
# is noticed. Exit status is the number of such surprises.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager
ALICE=$(token alice '[]')

if ! command -v oc >/dev/null; then
  curl -sfL https://mirror.openshift.com/pub/openshift-v4/x86_64/clients/ocp/stable/openshift-client-linux.tar.gz \
    | tar -xz -C "$W" oc || exit 100
  OC=$W/oc
else
  OC=$(command -v oc)
fi
"$OC" version --client | head -1

as() { # <token> <kubeconfig-name> oc-args...
  local t=$1 k=$2; shift 2
  # Capped here, not in verb(): `timeout` runs programs, not shell functions.
  KUBECONFIG=$W/kc-$k timeout 60 "$OC" --cache-dir "$W/cache-$k" --server "$API" \
    --insecure-skip-tls-verify --token "$t" "$@" 2>&1
}
adm() { as "$ADMIN" admin "$@"; }
# <works|missing> <verb label> <command...>
verb() {
  local want=$1 label=$2 out rc got; shift 2
  out=$("$@"); rc=$?
  [ $rc -eq 0 ] && got=works || got=missing
  local first; first=$(grep -m1 -iE 'error|forbidden|not found|could not|unable|doesn.t have' <<<"$out" | cut -c1-160)
  if [ "$got" = "$want" ]; then
    pass "$label: $got${first:+ — $first}"
  else
    fail "$label: expected $want, got $got (rc=$rc): $(tail -3 <<<"$out" | tr '\n' ' ' | cut -c1-300)"
  fi
}
check() { # <label> <command...> — a post-condition
  local label=$1; shift
  if out=$("$@" 2>&1); then pass "$label"; else fail "$label: $out"; fi
}

# --- a pretend cluster: two Ready nodes, pods bound to the first ------------
for n in n1 n2; do
  adm create -f - >/dev/null <<YAML
apiVersion: v1
kind: Node
metadata: { name: $n, labels: { kubernetes.io/hostname: $n } }
YAML
  adm patch node $n --subresource=status --type=merge \
    -p '{"status":{"conditions":[{"type":"Ready","status":"True","reason":"KubeletReady"}]}}' >/dev/null
done
adm create namespace work >/dev/null
for p in p1 p2; do
  adm create -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: { name: $p, namespace: work }
spec: { nodeName: n1, containers: [ { name: c, image: registry.invalid/x } ] }
YAML
done

# --- node lifecycle -----------------------------------------------------------
verb works "cordon" adm adm cordon n1
check "cordon: n1 unschedulable" test "$(adm get node n1 -o jsonpath='{.spec.unschedulable}')" = true
verb works "uncordon" adm adm uncordon n1
check "uncordon: n1 schedulable" test -z "$(adm get node n1 -o jsonpath='{.spec.unschedulable}')"
verb works "taint" adm adm taint node n2 dedicated=infra:NoSchedule
check "taint: on n2" test "$(adm get node n2 -o jsonpath='{.spec.taints[0].key}')" = dedicated
verb works "taint (remove)" adm adm taint node n2 dedicated-
verb works "drain" adm adm drain n1 --force --ignore-daemonsets --delete-emptydir-data --timeout=45s
check "drain: n1 cordoned and empty" test "$(adm get node n1 -o jsonpath='{.spec.unschedulable}')/$(adm get pods -n work -o name | wc -l)" = "true/0"
# nodes/n1/proxy is served (#108); there is no kubelet here, and the
# kubelet serves no /logs/ yet (rustkube-node#198).
verb missing "node-logs" adm adm node-logs n1
# metrics.k8s.io is served from each node's cadvisor (#89): the stand-in
# nodes have none, so a node has no metrics; the Pods list answers (empty).
verb missing "top node" adm adm top node
verb works "top pod" adm adm top pod -n work

# --- policy -------------------------------------------------------------------
# authorization.openshift.io LocalResourceAccessReview, not the k8s SAR (#106)
verb works "policy who-can" adm adm policy who-can get pods -n work
check "who-can lists system:masters" bash -c "KUBECONFIG=$W/kc-admin timeout 60 $OC --cache-dir $W/cache-admin --server $API --insecure-skip-tls-verify --token $ADMIN adm policy who-can get pods -n work | grep -q system:masters"
verb works "policy add-role-to-user" adm adm policy add-role-to-user edit alice -n work
check "alice can now list pods in work" as "$ALICE" alice get pods -n work
verb works "policy add-role-to-group" adm adm policy add-role-to-group view devs -n work
# The remove verbs read bindings as protobuf and match on roleRef.kind,
# which the encoder used to drop (#69).
verb works "policy remove-role-from-user" adm adm policy remove-role-from-user edit alice -n work
check "alice can no longer list pods in work" bash -c "! KUBECONFIG=$W/kc-alice timeout 60 $OC --cache-dir $W/cache-alice --server $API --insecure-skip-tls-verify --token $ALICE get pods -n work >/dev/null 2>&1"
verb works "policy remove-role-from-group" adm adm policy remove-role-from-group view devs -n work
verb works "policy add-cluster-role-to-user" adm adm policy add-cluster-role-to-user view alice
check "alice can now list namespaces" as "$ALICE" alice get ns
verb works "policy remove-cluster-role-from-user" adm adm policy remove-cluster-role-from-user view alice
verb works "policy add-cluster-role-to-group" adm adm policy add-cluster-role-to-group view devs
verb works "policy remove-cluster-role-from-group" adm adm policy remove-cluster-role-from-group view devs
adm adm policy add-role-to-user view alice -n work >/dev/null
verb works "policy remove-user" adm adm policy remove-user alice -n work
adm adm policy add-role-to-group view devs -n work >/dev/null
verb works "policy remove-group" adm adm policy remove-group devs -n work
# Binds ClusterRole system:openshift:scc:privileged, which SCC admission
# reads (#321: alice may then use the privileged SCC; scc-restricted.sh).
verb works "policy add-scc-to-user" adm adm policy add-scc-to-user privileged alice
# security.openshift.io (#373): the SCCs as objects, and the reviews
# answered by SCC admission's providers.
OCA() { KUBECONFIG=$W/kc-admin timeout 60 "$OC" --cache-dir "$W/cache-admin" --server "$API" --insecure-skip-tls-verify --token "$ADMIN" "$@" 2>&1; }
has() { # <extended regex> <oc args...> — the admin's output has a matching line
  local re=$1; shift
  OCA "$@" | grep -Eq -- "$re"
}
check "get scc: the three built-ins" test "$(OCA get scc --no-headers | awk '{print $1}' | sort | tr '\n' ' ')" = "anyuid privileged restricted-v2 "
check "get scc: OpenShift's columns" has '^NAME +PRIV +CAPS +SELINUX +RUNASUSER +FSGROUP +SUPGROUP +PRIORITY +READONLYROOTFS +VOLUMES' get scc
check "get scc restricted-v2: NET_BIND_SERVICE, MustRunAsRange, <no value>" \
  has '^restricted-v2 +false +\["NET_BIND_SERVICE"\] +MustRunAs +MustRunAsRange +MustRunAs +RunAsAny +<no value> +false' get scc restricted-v2 --no-headers
verb works "describe scc" adm describe scc anyuid
printf '%s' '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"x"},"spec":{"containers":[{"name":"c","image":"i"}]}}' >"$W/scc-pod.json"
verb works "policy scc-subject-review -u" adm adm policy scc-subject-review -u alice -n work -f "$W/scc-pod.json"
check "scc-subject-review -u alice: privileged (add-scc-to-user above)" has '^Pod/x +privileged' adm policy scc-subject-review -u alice -n work -f "$W/scc-pod.json"
check "scc-subject-review (self, cluster-admin): anyuid" has '^Pod/x +anyuid' adm policy scc-subject-review -n work -f "$W/scc-pod.json"
check "scc-subject-review -u bob -g system:authenticated: restricted-v2" \
  has '^Pod/x +restricted-v2' adm policy scc-subject-review -u bob -g system:authenticated -n work -f "$W/scc-pod.json"
adm create sa builder -n work >/dev/null
verb works "policy add-scc-to-user -z" adm adm policy add-scc-to-user anyuid -z builder -n work
verb works "policy scc-review -z" adm adm policy scc-review -z builder -n work -f "$W/scc-pod.json"
check "scc-review -z builder: Pod/x builder anyuid" has '^Pod/x +builder +anyuid' adm policy scc-review -z builder -n work -f "$W/scc-pod.json"
check "scc-review -z ghost: no such ServiceAccount" has 'unable to retrieve ServiceAccount ghost' adm policy scc-review -z ghost -n work -f "$W/scc-pod.json"
check "a user may not create an SCC" bash -c "! KUBECONFIG=$W/kc-alice timeout 60 $OC --cache-dir $W/cache-alice --server $API --insecure-skip-tls-verify --token $ALICE create -f - <<<'{\"apiVersion\":\"security.openshift.io/v1\",\"kind\":\"SecurityContextConstraints\",\"metadata\":{\"name\":\"mine\"},\"runAsUser\":{\"type\":\"RunAsAny\"},\"seLinuxContext\":{\"type\":\"RunAsAny\"}}' >/dev/null 2>&1"

# --- certificates -------------------------------------------------------------
for c in csr-ok csr-no; do
  openssl req -new -newkey rsa:2048 -nodes -keyout "$W/$c.key" -subj "/CN=$c/O=test" -out "$W/$c.csr" 2>/dev/null
  adm create -f - >/dev/null <<YAML
apiVersion: certificates.k8s.io/v1
kind: CertificateSigningRequest
metadata: { name: $c }
spec:
  request: $(base64 -w0 "$W/$c.csr")
  signerName: kubernetes.io/kube-apiserver-client
  usages: [ client auth ]
YAML
done
verb works "certificate approve" adm adm certificate approve csr-ok
check "csr-ok is Approved" test "$(adm get csr csr-ok -o jsonpath='{.status.conditions[0].type}')" = Approved
verb works "certificate deny" adm adm certificate deny csr-no
check "csr-no is Denied" test "$(adm get csr csr-no -o jsonpath='{.status.conditions[0].type}')" = Denied

# --- projects and groups ------------------------------------------------------
# Its post-check is an authorization.openshift.io SubjectAccessReview of
# `get projects` (no group) as alice (#106).
verb works "new-project" adm adm new-project team --admin=alice --display-name=Team
check "team: alice is its admin" as "$ALICE" alice get project team
for t in create-bootstrap-project-template create-login-template create-error-template create-provider-selection-template; do
  verb works "$t" adm adm $t
done
# user.openshift.io Groups (#372): a Group's users carry it at
# authentication, so a binding to the Group grants them.
BOB=$(token bob '[]')
CAROL_SA=system:serviceaccount:storm-users:carol
CAROL=$(token "$CAROL_SA" '[]')
can() { # <token> <name> <verb> <resource> <ns> — SelfSubjectAccessReview
  KUBECONFIG=$W/kc-$2 timeout 60 "$OC" --cache-dir "$W/cache-$2" --server "$API" \
    --insecure-skip-tls-verify --token "$1" auth can-i "$3" "$4" -n "$5" 2>/dev/null
}
verb works "groups new" adm adm groups new team-devs alice
check "groups new: says created" bash -c "KUBECONFIG=$W/kc-admin timeout 60 $OC --cache-dir $W/cache-admin --server $API --insecure-skip-tls-verify --token $ADMIN adm groups new team-ops 2>&1 | grep -qx 'group.user.openshift.io/team-ops created'"
check "groups new: users [alice]" test "$(adm get groups team-devs -o jsonpath='{.users}')" = '["alice"]'
check "groups new: an empty group's users are null" test "$(adm get group team-ops -o json | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["users"])')" = None
verb works "groups add-users" adm adm groups add-users team-devs bob "$CAROL_SA"
check "add-users: alice, bob and the ServiceAccount" test "$(adm get groups team-devs -o jsonpath='{.users}')" = "[\"alice\",\"bob\",\"$CAROL_SA\"]"
check "get groups: USERS column" bash -c "KUBECONFIG=$W/kc-admin timeout 60 $OC --cache-dir $W/cache-admin --server $API --insecure-skip-tls-verify --token $ADMIN get groups team-devs --no-headers | grep -q 'alice, bob, $CAROL_SA'"
check "bob may not list pods in work yet" test "$(can "$BOB" bob list pods work)" = no
adm adm policy add-role-to-group view team-devs -n work >/dev/null
check "a binding to team-devs grants bob (SSAR)" test "$(can "$BOB" bob list pods work)" = yes
check "and the ServiceAccount member (SSAR)" test "$(can "$CAROL" carol list pods work)" = yes
check "bob may not delete pods (view only)" test "$(can "$BOB" bob delete pods work)" = no
check "SelfSubjectReview: bob is in team-devs" bash -c "curl -sk -H 'Authorization: Bearer $BOB' -H 'Content-Type: application/json' -X POST -d '{\"apiVersion\":\"authentication.k8s.io/v1\",\"kind\":\"SelfSubjectReview\"}' $API/apis/authentication.k8s.io/v1/selfsubjectreviews | grep -q '\"team-devs\"'"
verb works "groups remove-users" adm adm groups remove-users team-devs bob
check "remove-users: bob gone" test "$(adm get groups team-devs -o jsonpath='{.users}')" = "[\"alice\",\"$CAROL_SA\"]"
check "bob's grant went with his membership (SSAR)" test "$(can "$BOB" bob list pods work)" = no
# OpenShift's validation: no ":" in a Group name, so no Group is a system: group.
verb missing "groups new system:masters (refused)" adm adm groups new system:masters bob
check "and nothing was stored" bash -c "! KUBECONFIG=$W/kc-admin timeout 60 $OC --cache-dir $W/cache-admin --server $API --insecure-skip-tls-verify --token $ADMIN get group system:masters >/dev/null 2>&1"
check "a user may not list groups" bash -c "! KUBECONFIG=$W/kc-alice timeout 60 $OC --cache-dir $W/cache-alice --server $API --insecure-skip-tls-verify --token $ALICE get groups >/dev/null 2>&1"
# `groups sync` and `prune groups` read an LDAP server, which this rig has
# none of. `prune groups` lists the Groups synced from the configured host
# (label openshift.io/ldap.host), finds none, and asks LDAP nothing.
cat >"$W/ldap-sync.yaml" <<'YAML'
kind: LDAPSyncConfig
apiVersion: v1
url: ldap://127.0.0.1:1
insecure: true
rfc2307:
  groupsQuery: { baseDN: "ou=groups,dc=example,dc=com", scope: sub, derefAliases: never, filter: "(objectClass=groupOfNames)" }
  groupUIDAttribute: dn
  groupNameAttributes: [ cn ]
  groupMembershipAttributes: [ member ]
  usersQuery: { baseDN: "ou=users,dc=example,dc=com", scope: sub, derefAliases: never }
  userUIDAttribute: dn
  userNameAttributes: [ uid ]
YAML
verb works "prune groups (no LDAP-synced groups)" adm adm prune groups --sync-config="$W/ldap-sync.yaml" --confirm
check "prune groups kept the groups it did not sync" test "$(adm get groups -o name | wc -l)" = 2
verb works "groups delete (oc delete group)" adm delete group team-ops

# --- a Deployment created by oc (protobuf) keeps its ReplicaSet (#99) ---------
adm create deployment web -n work --image=registry.invalid/web >/dev/null
check "deployment web has a uid" test -n "$(adm get deployment web -n work -o jsonpath='{.metadata.uid}')"
sleep 45   # several GC passes
check "its ReplicaSet was never collected as orphaned" bash -c "! grep -q 'ReplicaSet work/web.*owner is gone' $W/cm.log"
check "and it is owned by the deployment's uid" test "$(adm get rs -n work -o jsonpath='{.items[0].metadata.ownerReferences[0].uid}')" = "$(adm get deployment web -n work -o jsonpath='{.metadata.uid}')"

# --- inspection ---------------------------------------------------------------
# Exits 0 now that /api and /apis answer aggregated discovery (#107). Its
# own namespace: in `work` a Pod drained off n1 is on no node, and gathering
# its container logs fails whatever discovery says.
adm create namespace inspected >/dev/null
verb works "inspect" adm adm inspect ns/inspected --dest-dir="$W/inspect"
check "inspect wrote its data" test -d "$W/inspect/namespaces/inspected"
check "aggregated discovery: /apis answers APIGroupDiscoveryList" bash -c "curl -sk -H 'Authorization: Bearer $ADMIN' -H 'Accept: application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList,application/json' -D - $API/apis | grep -qi 'content-type: application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList'"
check "aggregated discovery: /api lists pods" bash -c "curl -sk -H 'Authorization: Bearer $ADMIN' -H 'Accept: application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList' $API/api | grep -q '\"resource\":\"pods\"'"
verb missing "must-gather" adm adm must-gather --dest-dir="$W/mg" --timeout=30s

# --- the OpenShift platform: out of scope here (#70) ------------------------
verb missing "upgrade" adm adm upgrade
verb missing "wait-for-stable-cluster" adm adm wait-for-stable-cluster --minimum-stable-period=1s --timeout=10s
verb missing "prune deployments" adm adm prune deployments
verb missing "prune builds" adm adm prune builds
verb missing "prune images" adm adm prune images
verb missing "build-chain" adm adm build-chain openshift/ruby
verb missing "reboot-machine-config-pool" adm adm reboot-machine-config-pool mcp/worker
verb missing "ocp-certificates regenerate-leaf" adm adm ocp-certificates regenerate-leaf -n openshift-config-managed secrets kube-controller-manager-client-cert-key

report
