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
verb missing "node-logs" adm adm node-logs n1
verb missing "top node" adm adm top node
verb missing "top pod" adm adm top pod -n work

# --- policy -------------------------------------------------------------------
# authorization.openshift.io LocalResourceAccessReview, not the k8s SAR (#106)
verb missing "policy who-can" adm adm policy who-can get pods -n work
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
# Binds ClusterRole system:openshift:scc:privileged. Accepted, and inert:
# there are no SCCs and no SCC admission here (#70).
verb works "policy add-scc-to-user (inert)" adm adm policy add-scc-to-user privileged alice
verb missing "policy scc-review" adm adm policy scc-subject-review -u alice -f - <<<'{"apiVersion":"v1","kind":"Pod","metadata":{"name":"x"},"spec":{"containers":[{"name":"c","image":"i"}]}}'

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
# Creates the project and the binding, then fails its own post-check, an
# authorization.openshift.io SubjectAccessReview (#106).
verb missing "new-project (post-check)" adm adm new-project team --admin=alice --display-name=Team
check "team: alice is its admin" as "$ALICE" alice get project team
for t in create-bootstrap-project-template create-login-template create-error-template create-provider-selection-template; do
  verb works "$t" adm adm $t
done
verb missing "groups new" adm adm groups new devs alice
verb missing "prune groups" adm adm prune groups --sync-config=/dev/null

# --- a Deployment created by oc (protobuf) keeps its ReplicaSet (#99) ---------
adm create deployment web -n work --image=registry.invalid/web >/dev/null
check "deployment web has a uid" test -n "$(adm get deployment web -n work -o jsonpath='{.metadata.uid}')"
sleep 45   # several GC passes
check "its ReplicaSet was never collected as orphaned" bash -c "! grep -q 'ReplicaSet work/web.*owner is gone' $W/cm.log"
check "and it is owned by the deployment's uid" test "$(adm get rs -n work -o jsonpath='{.items[0].metadata.ownerReferences[0].uid}')" = "$(adm get deployment web -n work -o jsonpath='{.metadata.uid}')"

# --- inspection ---------------------------------------------------------------
# Writes its data, then exits 1: /apis has no aggregated discovery (#107).
verb missing "inspect" adm adm inspect ns/work --dest-dir="$W/inspect"
check "inspect wrote its data" test -d "$W/inspect/namespaces/work"
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
