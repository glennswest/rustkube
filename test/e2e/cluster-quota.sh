#!/usr/bin/env bash
#
# quota.openshift.io ClusterResourceQuotas (#391, sc#50), with a real `oc`
# against a real apiserver and controller-manager on fastetcd. No kubelet:
# Pods are API objects and stay Pending, which is all a pods quota counts.
#
# - the issue's proof: `oc create clusterresourcequota q
#   --project-label-selector=team=a --hard=pods=1`; one pod in a `team=a`
#   namespace is admitted, a second — in another `team=a` namespace — is
#   refused with quota's words; a namespace without the label is not held;
#   `oc get clusterresourcequota q -o yaml` shows the usage (total and per
#   namespace)
# - a deleted pod's usage is given back by the controller, and the next pod
#   is admitted
# - an annotation selector; validation (no selector); `oc get`/`describe`
# - AppliedClusterResourceQuotas: listed and described in a selected
#   namespace, absent elsewhere; a project viewer reads them and not the
#   ClusterResourceQuotas
#
# Exit status is the number of failed checks.
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
  KUBECONFIG=$W/kc-$k timeout 60 "$OC" --cache-dir "$W/cache-$k" --server "$API" \
    --insecure-skip-tls-verify --token "$t" "$@" 2>&1
}
adm() { as "$ADMIN" admin "$@"; }
check() { # <label> <command...>
  local label=$1; shift
  if out=$("$@" 2>&1); then pass "$label"; else fail "$label: $(tr '\n' ' ' <<<"$out" | cut -c1-300)"; fi
}
refused() { # <label> <expected output regex> <command...>
  local label=$1 re=$2 out; shift 2
  if out=$("$@"); then fail "$label: expected a refusal, got: $(tr '\n' ' ' <<<"$out" | cut -c1-200)"
  elif grep -Eq -- "$re" <<<"$out"; then pass "$label"
  else fail "$label: refused, but not with /$re/: $(tr '\n' ' ' <<<"$out" | cut -c1-300)"; fi
}
eq() { # <label> <want> <got>
  if [ "$2" = "$3" ]; then pass "$1"; else fail "$1: want '$2', got '$3'"; fi
}
has() { # <extended regex> <oc args...> — the admin's output has a matching line
  local re=$1; shift
  adm "$@" | grep -Eq -- "$re"
}
until_eq() { # <seconds> <want> <command...> — poll until the output is <want>
  local n=$1 want=$2 got=""; shift 2
  for _ in $(seq "$n"); do got=$("$@" 2>/dev/null); [ "$got" = "$want" ] && { echo "$got"; return 0; }; sleep 1; done
  echo "$got"; return 1
}
pod() { # <namespace> <name> [token kubeconfig]
  local t=${3:-$ADMIN} k=${4:-admin}
  as "$t" "$k" create -n "$1" -f - <<YAML
apiVersion: v1
kind: Pod
metadata: { name: $2 }
spec: { containers: [ { name: c, image: registry.invalid/x } ] }
YAML
}

for ns in team-a1 team-a2 other; do adm create namespace $ns >/dev/null; done
adm label namespace team-a1 team=a >/dev/null
adm label namespace team-a2 team=a >/dev/null

# --- the issue's proof -------------------------------------------------------
check "oc create clusterresourcequota q --project-label-selector=team=a --hard=pods=1" \
  adm create clusterresourcequota q --project-label-selector=team=a --hard=pods=1
eq "the controller computes q's status" '{"pods":"1"}' \
  "$(until_eq 60 '{"pods":"1"}' adm get clusterresourcequota q -o jsonpath='{.status.total.hard}')"
check "a pod in team-a1 is admitted" pod team-a1 p1
refused "a second pod, in team-a2, is refused with quota's words" \
  'forbidden: exceeded quota: q, requested: pods=1, used: pods=1, limited: pods=1' pod team-a2 p2
check "a namespace the quota does not select is not held" pod other p3
adm get clusterresourcequota q -o yaml >"$W/q.yaml"
check "oc get clusterresourcequota q -o yaml shows the usage" python3 -I -c '
import json, sys
q = json.loads(sys.stdin.read())
t = q["status"]["total"]
assert t["hard"] == {"pods": "1"} and t["used"] == {"pods": "1"}, t
byns = {e["namespace"]: e["status"]["used"] for e in q["status"]["namespaces"]}
assert byns == {"team-a1": {"pods": "1"}, "team-a2": {"pods": "0"}}, byns
' <<<"$(adm get clusterresourcequota q -o json)"
check "and the yaml names it" grep -Eq '^ +namespace: team-a1$' "$W/q.yaml"

# --- usage given back ------------------------------------------------------------
check "delete the pod in team-a1" adm delete pod p1 -n team-a1 --grace-period=0 --force
eq "the controller gives its usage back" '{"pods":"0"}' \
  "$(until_eq 60 '{"pods":"0"}' adm get clusterresourcequota q -o jsonpath='{.status.total.used}')"
check "now a pod in team-a2 is admitted" pod team-a2 p2

# --- printing, annotations, validation ---------------------------------------------
check "oc get clusterresourcequota: OpenShift's columns" has '^NAME +LABELS SELECTOR +ANNOTATIONS SELECTOR' get clusterresourcequota
check "oc get clusterresourcequota q: team=a, <none>" has '^q +team=a +<none>$' get clusterresourcequota q --no-headers
check "oc get clusterquota (short name)" adm get clusterquota q
check "oc describe clusterresourcequota q" has 'pods' describe clusterresourcequota q
adm annotate namespace other openshift.io/requester=alice >/dev/null
check "an annotation selector" adm create clusterresourcequota by-requester \
  --project-annotation-selector=openshift.io/requester=alice --hard=count/configmaps=5
eq "it selects the annotated namespace" other \
  "$(until_eq 60 other adm get clusterresourcequota by-requester -o jsonpath='{.status.namespaces[*].namespace}')"
refused "a quota that selects nothing is refused" 'must restrict the selected projects' \
  adm create clusterresourcequota everything --hard=pods=1

# --- AppliedClusterResourceQuotas ------------------------------------------------------
check "oc get appliedclusterresourcequota -n team-a1 lists q" has '^q ' get appliedclusterresourcequota -n team-a1 --no-headers
check "oc describe appliedclusterresourcequota q -n team-a1" has 'pods' describe appliedclusterresourcequota q -n team-a1
refused "q is not applied in other" 'not found' adm get appliedclusterresourcequota q -n other
adm adm policy add-role-to-user view alice -n team-a1 >/dev/null
check "a viewer of team-a1 reads its applied quotas" as "$ALICE" alice get appliedclusterresourcequota q -n team-a1
refused "and not the ClusterResourceQuotas" 'forbidden' as "$ALICE" alice get clusterresourcequota q
report
