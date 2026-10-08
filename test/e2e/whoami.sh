#!/usr/bin/env bash
#
# SelfSubjectReview, "who am I" (#116), against a real apiserver on fastetcd.
#
# - discovery lists authentication.k8s.io/v1 selfsubjectreviews
# - `kubectl auth whoami` as the admin and as a plain user prints the name
#   and the groups, system:authenticated included
# - a raw POST answers 201 with status.userInfo
# - the plain user needs no binding beyond system:basic-user
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
KUBECTL=${RK_TOOLS:+$RK_TOOLS/kubectl}
[ -x "${KUBECTL:-}" ] || KUBECTL=$(command -v kubectl || true)
if [ -z "$KUBECTL" ]; then
  KUBECTL=$W/kubectl
  curl -sfL -o "$KUBECTL" "https://dl.k8s.io/$(curl -sfL "https://dl.k8s.io/release/$KUBECTL_CHANNEL.txt")/bin/linux/amd64/kubectl" || exit 100
  chmod +x "$KUBECTL"
fi
ALICE=$(token alice '["devs"]')
whoami() { "$KUBECTL" --server "$API" --insecure-skip-tls-verify --token "$1" auth whoami -o json 2>&1; }
curl -sk -H "Authorization: Bearer $ADMIN" "$API/apis/authentication.k8s.io/v1" | grep -q '"selfsubjectreviews"' \
  && pass "discovery lists selfsubjectreviews" || fail "discovery lists selfsubjectreviews"
out=$(whoami "$ALICE")
python3 -c 'import json,sys; u=json.loads(sys.argv[1])["status"]["userInfo"]; assert u["username"]=="alice" and "devs" in u["groups"] and "system:authenticated" in u["groups"], u' "$out" \
  && pass "kubectl auth whoami as alice: name, devs, system:authenticated" || fail "kubectl auth whoami as alice ($out)"
out=$(whoami "$ADMIN")
python3 -c 'import json,sys; u=json.loads(sys.argv[1])["status"]["userInfo"]; assert u["username"]=="admin" and "system:masters" in u["groups"], u' "$out" \
  && pass "kubectl auth whoami as the admin" || fail "kubectl auth whoami as the admin ($out)"
code=$(curl -sk -o "$W/ssr" -w '%{http_code}' -X POST -H "Authorization: Bearer $ALICE" -H 'Content-Type: application/json' \
  -d '{"apiVersion":"authentication.k8s.io/v1","kind":"SelfSubjectReview"}' "$API/apis/authentication.k8s.io/v1/selfsubjectreviews")
[ "$code" = 201 ] && grep -q '"username":"alice"' "$W/ssr" && pass "raw POST: 201 with status.userInfo" || fail "raw POST ($code $(cat "$W/ssr"))"
report
