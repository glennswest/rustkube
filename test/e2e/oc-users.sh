#!/usr/bin/env bash
#
# user.openshift.io Users, Identities and UserIdentityMappings (#390, sc#50),
# with a real `oc` against a real apiserver on fastetcd:
#
# - discovery lists users, identities and useridentitymappings (the last
#   create/get/update/delete only)
# - the issue's proof: `oc create user u1 --full-name=U`, `oc create identity
#   p:login`, `oc create useridentitymapping p:login u1`, and `oc get user u1`
#   lists p:login under identities; the Identity names u1
# - a second mapping of the same Identity is AlreadyExists; a PUT moves it to
#   another User (the first lets it go); delete unmaps it, both objects stay
# - OpenShift's validation (a User named `a:b`, a mapping to no User)
# - `oc get users|identities|useridentitymapping` print OpenShift's columns
# - `oc whoami` (users/~) for the admin and for a user with no User object
#   (basic-user's `get users ~`); that user may not create Users
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
ALICE=$(token alice '["devs"]')

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
refused() { # <label> <expected output regex> <command...> — fails, saying why
  local label=$1 re=$2 out; shift 2
  if out=$("$@"); then fail "$label: expected a refusal, got: $(tr '\n' ' ' <<<"$out" | cut -c1-200)"
  elif grep -Eq -- "$re" <<<"$out"; then pass "$label"
  else fail "$label: refused, but not with /$re/: $(tr '\n' ' ' <<<"$out" | cut -c1-300)"; fi
}
eq() { # <label> <want> <got>
  if [ "$2" = "$3" ]; then pass "$1"; else fail "$1: want '$2', got '$3'"; fi
}
api() { # <method> <path> [body] — prints the HTTP code, body to $W/api.out
  curl -sk -o "$W/api.out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H 'Content-Type: application/json' ${3:+-d "$3"} "$API$2"
}

disc=$(curl -sk -H "Authorization: Bearer $ADMIN" "$API/apis/user.openshift.io/v1")
eq "discovery: users, identities, useridentitymappings, groups" "groups identities useridentitymappings users" \
  "$(python3 -I -c 'import json,sys; print(" ".join(sorted(r["name"] for r in json.load(sys.stdin)["resources"])))' <<<"$disc")"
eq "discovery: useridentitymappings create/delete/get/update" "create delete get update" \
  "$(python3 -I -c 'import json,sys; print(" ".join(next(r["verbs"] for r in json.load(sys.stdin)["resources"] if r["name"]=="useridentitymappings")))' <<<"$disc")"

# --- the issue's proof -------------------------------------------------------
check "oc create user u1 --full-name=U" adm create user u1 --full-name=U
check "oc create identity p:login" adm create identity p:login
eq "the Identity is unmapped" "" "$(adm get identity p:login -o jsonpath='{.user.name}')"
check "oc create useridentitymapping p:login u1" adm create useridentitymapping p:login u1
adm get user u1 -o yaml >"$W/u1.yaml"
check "oc get user u1 -o yaml lists p:login under identities" bash -c "grep -A1 '^identities:' '$W/u1.yaml' | grep -qx -- '- p:login'"
check "and fullName U" grep -qx 'fullName: U' "$W/u1.yaml"
eq "the Identity names u1" "u1/$(adm get user u1 -o jsonpath='{.metadata.uid}')" \
  "$(adm get identity p:login -o jsonpath='{.user.name}/{.user.uid}')"
eq "oc get useridentitymapping p:login: user u1" "u1" "$(adm get useridentitymapping p:login -o jsonpath='{.user.name}')"
eq "a User's groups are null" "None" "$(adm get user u1 -o json | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["groups"])')"

# --- printers ------------------------------------------------------------------
has() { # <extended regex> <oc args...> — the admin's output has a matching line
  local re=$1; shift
  adm "$@" | grep -Eq -- "$re"
}
check "oc get users: OpenShift's columns" has '^NAME +UID +FULL NAME +IDENTITIES' get users
check "oc get users: u1, U, p:login" has '^u1 +[^ ]+ +U +p:login$' get users --no-headers
check "oc get identities: OpenShift's columns" has '^NAME +IDP NAME +IDP USER NAME +USER NAME +USER UID' get identities
check "oc get identities: p:login p login u1" has '^p:login +p +login +u1 ' get identities --no-headers
check "oc get useridentitymapping: p:login p:login u1" has '^p:login +p:login +u1 ' get useridentitymapping p:login --no-headers

# --- the mapping's other verbs ---------------------------------------------------
refused "a second mapping of p:login: AlreadyExists" 'already exists' adm create useridentitymapping p:login u1
check "oc create user u2" adm create user u2
code=$(api PUT /apis/user.openshift.io/v1/useridentitymappings/p:login \
  '{"apiVersion":"user.openshift.io/v1","kind":"UserIdentityMapping","metadata":{"name":"p:login"},"identity":{"name":"p:login"},"user":{"name":"u2"}}')
eq "PUT the mapping to u2: 200" 200 "$code"
eq "u1 lets p:login go" "" "$(adm get user u1 -o jsonpath='{.identities}')"
eq "u2 lists p:login" '["p:login"]' "$(adm get user u2 -o jsonpath='{.identities}')"
eq "the Identity names u2" u2 "$(adm get identity p:login -o jsonpath='{.user.name}')"
eq "a mapping cannot be listed (405)" 405 "$(api GET /apis/user.openshift.io/v1/useridentitymappings)"
check "oc delete useridentitymapping p:login" adm delete useridentitymapping p:login
eq "u2 no longer lists it" "" "$(adm get user u2 -o jsonpath='{.identities}')"
eq "the Identity is unmapped again" "" "$(adm get identity p:login -o jsonpath='{.user.name}')"
refused "the mapping is gone" 'not found' adm get useridentitymapping p:login
check "both objects stay" adm get user/u2 identity/p:login

# --- validation -------------------------------------------------------------------
refused "a User named a:b" 'usernames that contain ":" must begin with "b64:"' adm create user a:b
refused "a mapping to no User" 'referenced user does not exist' adm create useridentitymapping p:login ghost
refused "a mapping of no Identity" 'referenced identity does not exist' adm create useridentitymapping p:ghost u1
code=$(api POST /apis/user.openshift.io/v1/identities '{"apiVersion":"user.openshift.io/v1","kind":"Identity","metadata":{"name":"x:y"},"providerName":"q","providerUserName":"bob"}')
eq "an Identity is named by provider and login, not the body" "201 q:bob" \
  "$code $(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["metadata"]["name"])' "$W/api.out")"

# --- users/~ -------------------------------------------------------------------------
eq "oc whoami (admin)" admin "$(adm whoami)"
eq "oc whoami (alice, no User object, basic-user only)" alice "$(as "$ALICE" alice whoami)"
check "alice's users/~ carries her groups" bash -c "curl -sk -H 'Authorization: Bearer $ALICE' $API/apis/user.openshift.io/v1/users/~ | python3 -I -c 'import json,sys; u=json.load(sys.stdin); assert u[\"metadata\"][\"name\"]==\"alice\" and \"devs\" in u[\"groups\"] and \"system:authenticated\" in u[\"groups\"], u'"
refused "alice may not read another User" 'forbidden' as "$ALICE" alice get user u1
refused "alice may not create a User" 'forbidden' as "$ALICE" alice create user mallory
report
