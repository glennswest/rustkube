#!/usr/bin/env bash
#
# Zero-touch onboarding (#388, stormcentral#1676), against a real apiserver
# and controller-manager on fastetcd:
#
#   1. the bootstrap signer publishes kube-public/cluster-info: a kubeconfig
#      naming the controller-manager's --apiserver and the rig's CA
#   2. a bootstrap token with usage-bootstrap-signing gets
#      jws-kubeconfig-<id>, a detached HS256 JWS that verifies against the
#      token's secret (checked here with Python's hmac, not rustkube's code);
#      a token without signing gets none; a deleted token's and an expired
#      token's signature is removed; a changed kubeconfig is put back
#   3. anonymous may GET cluster-info, and no other ConfigMap there, no LIST
#   4. a bootstrapper in system:bootstrappers:stormcentral files a
#      kube-apiserver-client CSR for CN=stormcentral (O=stormcentral:forge),
#      reads it and no other; any other subject, a SAN, a server usage, or a
#      bootstrapper outside that group: 403
#
# Exit status is the number of failed checks.
RK_ANONYMOUS_AUTH=true
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
start_controller_manager

CM=/api/v1/namespaces/kube-public/configmaps/cluster-info
CSRS=/apis/certificates.k8s.io/v1/certificatesigningrequests
api() { # <token|-> <method> <path> [json] — body to $W/out, prints the code; - is anonymous
  local auth=()
  [ "$1" = - ] || auth=(-H "Authorization: Bearer $1")
  curl -s -o "$W/out" -w '%{http_code}' --cacert "$W/ca.crt" -X "$2" "${auth[@]}" \
    -H 'Content-Type: application/json' "$API$3" ${4:+-d "$4"}
}
expect() { # <want> <got> <what>
  [ "$2" = "$1" ] && pass "$3" || fail "$3: got $2, want $1 ($(head -c 200 "$W/out"))"
}
iso() { date -u -d "$1" +%Y-%m-%dT%H:%M:%SZ; }
secret() { # <id> <secret> <extra stringData json, without braces>
  api "$ADMIN" POST /api/v1/namespaces/kube-system/secrets "{\"apiVersion\":\"v1\",\"kind\":\"Secret\",
    \"metadata\":{\"name\":\"bootstrap-token-$1\"},\"type\":\"bootstrap.kubernetes.io/token\",
    \"stringData\":{\"token-id\":\"$1\",\"token-secret\":\"$2\",\"usage-bootstrap-authentication\":\"true\"${3:+,$3}}}"
}
# verify <id> <secret>: prints ok when cluster-info (in $W/out) carries a
# jws-kubeconfig-<id> that is the detached JWS of its kubeconfig; else why not.
verify() {
  python3 - "$1" "$2" "$W/out" <<'PY'
import base64, hashlib, hmac, json, sys
tid, sec, path = sys.argv[1:]
d = json.load(open(path)).get("data", {})
sig = d.get("jws-kubeconfig-" + tid)
if sig is None:
    print("absent"); sys.exit()
e = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()
h = e(json.dumps({"alg": "HS256", "kid": tid}, separators=(",", ":")).encode())
p = e(d["kubeconfig"].encode())
want = h + ".." + e(hmac.new(sec.encode(), (h + "." + p).encode(), hashlib.sha256).digest())
print("ok" if sig == want else "mismatch: %s != %s" % (sig, want))
PY
}
wait_for() { # <seconds> <command…>: until the command succeeds
  local n=$1; shift
  for _ in $(seq "$n"); do "$@" && return 0; sleep 1; done
  return 1
}
signed() { api "$ADMIN" GET $CM >/dev/null; [ "$(verify "$1" "$2")" = "$3" ]; }

# 1.
wait_for 60 eval '[ "$(api "$ADMIN" GET $CM)" = 200 ]' || fail "cluster-info never appeared ($(tail -20 "$W/cm.log"))"
python3 - "$W/out" "$API" "$W/ca.crt" <<'PY' && pass "cluster-info kubeconfig names the apiserver and the CA" || fail "cluster-info kubeconfig"
import base64, json, sys
d = json.load(open(sys.argv[1]))["data"]["kubeconfig"]
ca = base64.b64encode(open(sys.argv[3], "rb").read()).decode()
assert "    server: %s\n" % sys.argv[2] in d, d
assert "    certificate-authority-data: %s\n" % ca in d, d
assert "kind: Config\n" in d, d
PY

# 2.
expect 201 "$(secret abcdef 0123456789abcdef '"usage-bootstrap-signing":"true"')" "setup: a signing token"
expect 201 "$(secret nosign 0123456789abcdef '"usage-bootstrap-signing":"false"')" "setup: a token that does not sign"
expect 201 "$(secret expire 0123456789abcdef "\"usage-bootstrap-signing\":\"true\",\"expiration\":\"$(iso '+60 seconds')\"")" \
  "setup: a signing token expiring in 60 s"
wait_for 30 signed abcdef 0123456789abcdef ok && pass "the signing token's jws-kubeconfig verifies" \
  || fail "jws-kubeconfig-abcdef: $(verify abcdef 0123456789abcdef)"
signed expire 0123456789abcdef ok && pass "the expiring token signs until it expires" \
  || fail "jws-kubeconfig-expire: $(verify expire 0123456789abcdef)"
expect absent "$(verify nosign 0123456789abcdef)" "a token without usage-bootstrap-signing does not sign"
api "$ADMIN" GET $CM >/dev/null
python3 -c "import json; d=json.load(open('$W/out')); d['data']['kubeconfig']='forged'; print(json.dumps(d))" >"$W/cm.json"
expect 200 "$(api "$ADMIN" PUT $CM "$(cat "$W/cm.json")")" "setup: kubeconfig overwritten"
wait_for 30 eval 'api "$ADMIN" GET $CM >/dev/null; ! grep -q forged "$W/out"' && pass "a changed kubeconfig is put back" \
  || fail "kubeconfig left forged"
signed abcdef 0123456789abcdef ok && pass "re-signed over the restored kubeconfig" \
  || fail "after restore: $(verify abcdef 0123456789abcdef)"
wait_for 90 signed expire 0123456789abcdef absent && pass "an expired token's signature is removed" \
  || fail "jws-kubeconfig-expire after expiry: $(verify expire 0123456789abcdef)"
expect 200 "$(api "$ADMIN" DELETE /api/v1/namespaces/kube-system/secrets/bootstrap-token-abcdef)" "setup: signing token deleted"
wait_for 30 signed abcdef 0123456789abcdef absent && pass "a deleted token's signature is removed" \
  || fail "jws-kubeconfig-abcdef after delete: $(verify abcdef 0123456789abcdef)"

# 3.
expect 200 "$(api - GET $CM)" "anonymous reads cluster-info"
expect 201 "$(api "$ADMIN" POST /api/v1/namespaces/kube-public/configmaps \
  '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"other"},"data":{"a":"b"}}')" "setup: another kube-public ConfigMap"
expect 403 "$(api - GET /api/v1/namespaces/kube-public/configmaps/other)" "anonymous cannot read another ConfigMap"
expect 403 "$(api - GET /api/v1/namespaces/kube-public/configmaps)" "anonymous cannot list kube-public ConfigMaps"
expect 403 "$(api - PUT $CM "$(cat "$W/cm.json")")" "anonymous cannot write cluster-info"

# 4.
expect 201 "$(secret scntrl 0123456789abcdef '"auth-extra-groups":"system:bootstrappers:stormcentral"')" \
  "setup: a stormcentral bootstrap token"
expect 201 "$(secret plain0 0123456789abcdef '')" "setup: a plain bootstrap token"
SC=scntrl.0123456789abcdef PLAIN=plain0.0123456789abcdef
csr() { # <name> <subject> <usages json> [san]
  openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$W/$1.key" -subj "$2" \
    ${4:+-addext "subjectAltName=DNS:$4"} 2>/dev/null | base64 -w0 >"$W/$1.b64"
  printf '{"apiVersion":"certificates.k8s.io/v1","kind":"CertificateSigningRequest","metadata":{"name":"%s"},
    "spec":{"signerName":"kubernetes.io/kube-apiserver-client","request":"%s","usages":%s}}' "$1" "$(cat "$W/$1.b64")" "$3"
}
U='["digital signature","key encipherment","client auth"]'
expect 201 "$(api $SC POST $CSRS "$(csr sc-1 /O=stormcentral:forge/CN=stormcentral "$U")")" \
  "stormcentral bootstrapper files CN=stormcentral, O=stormcentral:forge"
python3 -c "import json; d=json.load(open('$W/out'))['spec']; assert d['username']=='system:bootstrap:scntrl', d; assert 'system:bootstrappers:stormcentral' in d['groups'], d" \
  && pass "stamped with the token's user and groups" || fail "stamp: $(head -c 300 "$W/out")"
expect 201 "$(api $SC POST $CSRS "$(csr sc-2 /CN=stormcentral '["client auth"]')")" "CN=stormcentral with no O"
expect 200 "$(api $SC GET $CSRS/sc-1)" "it reads its own CSR"
expect 201 "$(api "$ADMIN" POST $CSRS "$(csr admins /CN=admin "$U")")" "setup: an admin's CSR"
expect 403 "$(api $SC GET $CSRS/admins)" "it cannot read another's CSR"
expect 403 "$(api $SC GET $CSRS)" "it cannot list CSRs"
expect 403 "$(api $SC POST $CSRS "$(csr r1 /CN=admin "$U")")" "refused: another CN"
expect 403 "$(api $SC POST $CSRS "$(csr r2 /O=system:masters/CN=stormcentral "$U")")" "refused: O=system:masters"
expect 403 "$(api $SC POST $CSRS "$(csr r3 /OU=x/CN=stormcentral "$U")")" "refused: an OU"
expect 403 "$(api $SC POST $CSRS "$(csr r4 /CN=stormcentral "$U" stormcentral.example)")" "refused: a SAN"
expect 403 "$(api $SC POST $CSRS "$(csr r5 /CN=stormcentral '["client auth","server auth"]')")" "refused: server auth"
expect 403 "$(api $SC POST $CSRS "$(csr r6 /CN=stormcentral "$U" | sed 's#kube-apiserver-client#kube-apiserver-client-kubelet#')")" \
  "refused: the kubelet client signer"
expect 403 "$(api $PLAIN POST $CSRS "$(csr r7 /O=stormcentral:forge/CN=stormcentral "$U")")" \
  "refused: a bootstrapper outside system:bootstrappers:stormcentral"

report
