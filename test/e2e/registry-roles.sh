#!/usr/bin/env bash
#
# The registry roles and the reviews a node's registry asks (#325,
# stormblock-registry#188), against a real apiserver on a real fastetcd.
#
# The registry's own object, `registry.storm.io/v1 Image` (a namespaced CRD,
# stormblock-registry#187), is covered by the same roles (#333).
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
# (API group image.openshift.io, or $SAR_GROUP)
sar() {
  req POST /apis/authorization.k8s.io/v1/subjectaccessreviews "{
    \"apiVersion\":\"authorization.k8s.io/v1\",\"kind\":\"SubjectAccessReview\",
    \"spec\":{\"user\":\"$1\",\"groups\":$2,\"resourceAttributes\":{
      \"namespace\":\"$3\",\"verb\":\"$4\",\"group\":\"${SAR_GROUP:-image.openshift.io}\",
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
# oc prints the role as `Kind:  ClusterRole` and `Name:  registry-editor` on
# lines of their own under `Role:`, and the subject as a row of the table.
out=$(as "$BOB" bob describe rolebinding registry-editor -n shop)
if grep -qE '^ +Kind: +ClusterRole$' <<<"$out" && grep -qE '^ +Name: +registry-editor$' <<<"$out" \
  && grep -qE '^ *ServiceAccount +alice +storm-users' <<<"$out"; then
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
# oc resolves the resource through discovery first; one it cannot find goes
# out as the literal `imagestreams.image.openshift.io` in the core group and
# is answered `no` (upstream kubectl does the same). On a node the registry
# serves image.openshift.io (stormblock-registry#187); here a CRD stands in
# for it. `/x` in can-i is a resource NAME — the subresource is a flag.
adm apply -f - >/dev/null <<'YAML'
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: imagestreams.image.openshift.io}
spec:
  group: image.openshift.io
  scope: Namespaced
  names: {plural: imagestreams, singular: imagestream, kind: ImageStream, listKind: ImageStreamList, shortNames: [is]}
  versions:
  - name: v1
    served: true
    storage: true
    schema: {openAPIV3Schema: {type: object, x-kubernetes-preserve-unknown-fields: true}}
YAML
sleep 2 # discovery shows the new group
can_i() { # <want yes|no> <what> <token> <name> <namespace> — stdout only; a warning is a failure
  local want=$1 what=$2 t=$3 k=$4 ns=$5 o e
  o=$(KUBECONFIG=$W/kc-$k timeout 60 "$OC" --cache-dir "$W/cache-$k" --server "$API" \
    --insecure-skip-tls-verify --token "$t" auth can-i update imagestreams.image.openshift.io \
    --subresource=layers -n "$ns" 2>"$W/can-i.err")
  e=$(cat "$W/can-i.err")
  if [ "$o" = "$want" ] && [ -z "$e" ]; then pass "$what: $o"; else fail "$what: want $want, got '$o' ${e:+(stderr: $e)}"; fi
}
can_i yes "alice: can-i push to shop" "$ALICE" alice shop
can_i no  "alice: can-i push to other" "$ALICE" alice other

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


# --- registry.storm.io Image: the registry's own object (#333) ---------------
# stormcos ships the CRD and binds kube-system/registry to system:registry
# (stormcos#1043); the rig does both as they will. Printer columns as
# stormblock-registry's deploy/crd/images.registry.storm.io.yaml.
adm apply -f - >/dev/null <<'YAML'
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: images.registry.storm.io}
spec:
  group: registry.storm.io
  scope: Namespaced
  names: {plural: images, singular: image, kind: Image, listKind: ImageList}
  versions:
  - name: v1
    served: true
    storage: true
    subresources: {status: {}}
    additionalPrinterColumns:
    - {name: Source, type: string, jsonPath: .spec.source.upstream}
    - {name: Digest, type: string, jsonPath: .status.digest}
    - {name: Nodes, type: string, jsonPath: .status.nodeNames}
    - {name: Age, type: date, jsonPath: .metadata.creationTimestamp}
    schema: {openAPIV3Schema: {type: object, x-kubernetes-preserve-unknown-fields: true}}
YAML
sleep 2 # discovery shows the new group
adm create serviceaccount registry -n kube-system >/dev/null
expect_ok "admin: add-cluster-role-to-user system:registry -z registry -n kube-system" \
  adm adm policy add-cluster-role-to-user system:registry -z registry -n kube-system
KREG=system:serviceaccount:kube-system:registry
KREG_GROUPS='["system:serviceaccounts","system:serviceaccounts:kube-system","system:authenticated"]'
export SAR_GROUP=registry.storm.io
expect_sar true  "kube-system:registry: update images/status in shop" "$KREG" "$KREG_GROUPS" shop update images status
expect_sar true  "kube-system:registry: patch images/status in shop" "$KREG" "$KREG_GROUPS" shop patch images status
for v in list watch create update; do
  expect_sar true "kube-system:registry: $v images in shop" "$KREG" "$KREG_GROUPS" shop "$v" images
done
expect_sar false "kube-system:registry: no delete of images" "$KREG" "$KREG_GROUPS" shop delete images
expect_sar true  "alice (registry-editor): delete images in shop" "$ALICE_USER" "$SA_GROUPS" shop delete images
expect_sar false "alice: no images in other" "$ALICE_USER" "$SA_GROUPS" other list images
expect_sar true  "carol (registry-viewer): list images in shop" system:serviceaccount:storm-users:carol "$SA_GROUPS" shop list images
expect_sar false "carol: no create of images in shop" system:serviceaccount:storm-users:carol "$SA_GROUPS" shop create images
unset SAR_GROUP

IMG='{"apiVersion":"registry.storm.io/v1","kind":"Image","metadata":{"name":"web-v1"},
  "spec":{"source":{"upstream":"quay.io/x/web@sha256:abc"},"placement":{"all":true}}}'
expect_ok "alice: create an Image in shop" as "$ALICE" alice create -n shop -f - <<<"$IMG"
req POST /api/v1/namespaces/storm-users/serviceaccounts/carol/token \
  '{"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","spec":{}}' >/dev/null
CAROL=$(jq_ 'd["status"]["token"]')
expect_denied "carol: may not create an Image in shop" as "$CAROL" carol create -n shop -f - <<<"${IMG/web-v1/web-v2}"
out=$(as "$ALICE" alice get images -n shop)
if grep -qE '^NAME +SOURCE +DIGEST +NODES +AGE$' <<<"$out" && grep -qE '^web-v1 +quay.io/x/web@sha256:abc ' <<<"$out"; then
  pass "alice: get images -n shop with the printer columns"
else
  fail "alice: get images -n shop: $out"
fi

report
