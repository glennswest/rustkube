#!/usr/bin/env bash
#
# The registry roles and the reviews a node's registry asks (#325,
# stormblock-registry#188), against a real apiserver on a real fastetcd.
#
# A registry login is the user's token: the registry asks TokenReview who it
# is, then a SubjectAccessReview in the repository's project for each
# request — `get` on `imagestreams/layers` to pull, `update` to push. The
# user is made as `sc user create` makes one: a ServiceAccount in
# `storm-users`, signed in by a TokenRequest. Roles are bound with
# `oc adm policy add-role-to-user`, by a project's admin, as OpenShift.
#
#   test/e2e/registry-roles.sh      # from the checkout; builds what it runs
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
BOB=$(token bob '[]')

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
expect_ok() { # <what> <cmd...>
  local what=$1; shift
  if out=$("$@"); then pass "$what"; else fail "$what: $out"; fi
}
expect_denied() { # <what> <cmd...>
  local what=$1; shift
  if out=$("$@"); then fail "$what: allowed: $out"
  elif grep -qiE 'forbidden|not allowed|cannot|may not' <<<"$out"; then pass "$what"
  else fail "$what: failed, but not as forbidden: $out"; fi
}
req() { # <method> <path> [json] — HTTP code; body in $W/out
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H 'Content-Type: application/json' "$API$2" ${3:+-d "$3"}
}
jq_() { python3 -c "import json,sys; d=json.load(open('$W/out')); print($1)"; }

# sar <user> <groups-json> <namespace> <verb> <resource> [subresource] → true/false
sar() {
  req POST /apis/authorization.k8s.io/v1/subjectaccessreviews "{
    \"apiVersion\":\"authorization.k8s.io/v1\",\"kind\":\"SubjectAccessReview\",
    \"spec\":{\"user\":\"$1\",\"groups\":$2,\"resourceAttributes\":{
      \"namespace\":\"$3\",\"verb\":\"$4\",\"group\":\"image.openshift.io\",
      \"resource\":\"$5\",\"subresource\":\"${6:-}\"}}}" >/dev/null
  jq_ 'str(d["status"].get("allowed", False)).lower()'
}
expect_sar() { # <want true|false> <what> sar-args...
  local want=$1 what=$2 got; shift 2
  got=$(sar "$@")
  if [ "$got" = "$want" ]; then pass "$what: allowed=$got"; else fail "$what: want allowed=$want, got $got: $(cat "$W/out")"; fi
}

# --- the roles are bootstrapped -------------------------------------------------
for r in registry-viewer registry-editor system:image-puller system:image-pusher system:registry; do
  expect_ok "ClusterRole $r exists" adm get clusterrole "$r"
done

# --- a user as `sc user create` makes one ------------------------------------
adm create namespace storm-users >/dev/null
adm create serviceaccount alice -n storm-users >/dev/null
adm create serviceaccount carol -n storm-users >/dev/null
c=$(req POST /api/v1/namespaces/storm-users/serviceaccounts/alice/token \
  '{"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","spec":{}}')
ALICE=$( { [ "$c" = 201 ] || [ "$c" = 200 ]; } && jq_ 'd["status"]["token"]')
[ -n "$ALICE" ] && pass "alice: TokenRequest ($c)" || fail "alice: TokenRequest $c: $(cat "$W/out")"
ALICE_USER=system:serviceaccount:storm-users:alice
SA_GROUPS='["system:serviceaccounts","system:serviceaccounts:storm-users","system:authenticated"]'

# TokenReview: what the registry does with `podman login -p <token>`.
req POST /apis/authentication.k8s.io/v1/tokenreviews \
  "{\"apiVersion\":\"authentication.k8s.io/v1\",\"kind\":\"TokenReview\",\"spec\":{\"token\":\"$ALICE\"}}" >/dev/null
got=$(jq_ 'd["status"].get("authenticated")'):$(jq_ 'd["status"].get("user",{}).get("username")')
[ "$got" = "True:$ALICE_USER" ] && pass "TokenReview: alice's token is $ALICE_USER" || fail "TokenReview: got $got: $(cat "$W/out")"
req POST /apis/authentication.k8s.io/v1/tokenreviews \
  '{"apiVersion":"authentication.k8s.io/v1","kind":"TokenReview","spec":{"token":"not-a-token"}}' >/dev/null
[ "$(jq_ 'd["status"].get("authenticated")')" = False ] && pass "TokenReview: a bad password is not authenticated" \
  || fail "TokenReview: bad token: $(cat "$W/out")"

# --- projects: bob owns shop, the admin makes other ---------------------------
expect_ok "bob: oc new-project shop" as "$BOB" bob new-project shop
expect_ok "admin: oc new-project other" adm new-project other

expect_sar false "alice: no push to shop before a role" "$ALICE_USER" "$SA_GROUPS" shop update imagestreams layers
expect_sar false "alice: no pull from shop before a role" "$ALICE_USER" "$SA_GROUPS" shop get imagestreams layers

# --- shop's admin shares the registry -----------------------------------------
expect_ok "bob: oc adm policy add-role-to-user registry-editor alice -n shop" \
  as "$BOB" bob adm policy add-role-to-user registry-editor "$ALICE_USER" -n shop
expect_denied "bob: may not bind registry-editor in a project he does not own" \
  as "$BOB" bob adm policy add-role-to-user registry-editor "$ALICE_USER" -n other
out=$(as "$BOB" bob describe rolebinding -n shop)
if grep -q 'ClusterRole/registry-editor\|ClusterRole.*registry-editor' <<<"$out" && grep -q 'alice' <<<"$out"; then
  pass "oc describe rolebinding: registry-editor for alice"
else
  fail "oc describe rolebinding: no registry-editor binding for alice: $out"
fi

expect_sar true  "alice: push to shop (registry-editor)" "$ALICE_USER" "$SA_GROUPS" shop update imagestreams layers
expect_sar true  "alice: pull from shop" "$ALICE_USER" "$SA_GROUPS" shop get imagestreams layers
expect_sar true  "alice: create an image stream in shop" "$ALICE_USER" "$SA_GROUPS" shop create imagestreams
expect_sar false "alice: no push to other" "$ALICE_USER" "$SA_GROUPS" other update imagestreams layers
expect_sar false "carol (no role): no pull from shop" system:serviceaccount:storm-users:carol "$SA_GROUPS" shop get imagestreams layers
expect_sar true  "admin: everything" admin '["system:masters"]' other update imagestreams layers

# `sc auth can-i` with the user's own token: SelfSubjectAccessReview.
out=$(as "$ALICE" alice auth can-i update imagestreams.image.openshift.io/layers -n shop)
[ "$out" = yes ] && pass "alice: can-i push to shop: yes" || fail "alice: can-i push to shop: $out"
out=$(as "$ALICE" alice auth can-i update imagestreams.image.openshift.io/layers -n other)
[ "$out" = no ] && pass "alice: can-i push to other: no" || fail "alice: can-i push to other: $out"

# --- registry-viewer: pull only ------------------------------------------------
expect_ok "bob: add-role-to-user registry-viewer carol -n shop" \
  as "$BOB" bob adm policy add-role-to-user registry-viewer system:serviceaccount:storm-users:carol -n shop
expect_sar true  "carol: pull from shop (registry-viewer)" system:serviceaccount:storm-users:carol "$SA_GROUPS" shop get imagestreams layers
expect_sar false "carol: no push to shop" system:serviceaccount:storm-users:carol "$SA_GROUPS" shop update imagestreams layers

# --- cross-project pull: system:image-puller ----------------------------------
OTHER_SA=system:serviceaccount:other:default
OTHER_GROUPS='["system:serviceaccounts","system:serviceaccounts:other","system:authenticated"]'
expect_sar false "other's pods: no pull from shop before the grant" "$OTHER_SA" "$OTHER_GROUPS" shop get imagestreams layers
expect_ok "bob: add-role-to-group system:image-puller system:serviceaccounts:other -n shop" \
  as "$BOB" bob adm policy add-role-to-group system:image-puller system:serviceaccounts:other -n shop
expect_sar true  "other's pods: pull from shop" "$OTHER_SA" "$OTHER_GROUPS" shop get imagestreams layers
expect_sar false "other's pods: still no push to shop" "$OTHER_SA" "$OTHER_GROUPS" shop update imagestreams layers

# --- system:registry: the node's own service, own project ---------------------
adm create serviceaccount registry -n shop >/dev/null
expect_ok "bob: add-role-to-user system:registry -z registry -n shop" \
  as "$BOB" bob adm policy add-role-to-user system:registry -z registry -n shop
REG=system:serviceaccount:shop:registry
REG_GROUPS='["system:serviceaccounts","system:serviceaccounts:shop","system:authenticated"]'
expect_sar true  "system:registry: writes images in shop" "$REG" "$REG_GROUPS" shop create images
expect_sar true  "system:registry: push to shop" "$REG" "$REG_GROUPS" shop update imagestreams layers
expect_sar false "system:registry: nothing in other" "$REG" "$REG_GROUPS" other create images

report
