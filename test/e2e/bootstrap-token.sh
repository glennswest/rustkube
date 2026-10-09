#!/usr/bin/env bash
#
# Bootstrap-token authentication and the forge-node CSR rules (#264,
# stormcert#78), against a real apiserver:
#
#   1. a bootstrap token (Secret kube-system/bootstrap-token-<id>) creates a
#      storm.io/forge-node CSR, stamped as system:bootstrap:<id> whatever the
#      body claims, and is refused any other signerName
#   2. it reads its own CSR (GET, a WATCH of that name) and no other, no LIST
#   3. an expired token, a wrong secret, a deleted Secret: 401
#   4. system:node:<n> creates a forge-node CSR (a renewal)
#   5. /approval needs `approve` and /status's certificate `sign` on the
#      signer: a user with only the CSR subresources is refused; bound to
#      system:storm:forge-node-signer it approves and signs forge-node CSRs,
#      and not a kubelet one
#   6. TokenReview answers a bootstrap token
#
# Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

CSRS=/apis/certificates.k8s.io/v1/certificatesigningrequests
api() { # <token> <method> <path> [json] — body to $W/out, prints the code
  curl -s -o "$W/out" -w '%{http_code}' --cacert "$W/ca.crt" -X "$2" -H "Authorization: Bearer $1" \
    -H 'Content-Type: application/json' "$API$3" ${4:+-d "$4"}
}
field() { python3 -c "import json,sys; d=json.load(open('$W/out')); print(eval('d'+sys.argv[1]))" "$1" 2>/dev/null; }
expect() { # <want> <got> <what>
  [ "$2" = "$1" ] && pass "$3" || fail "$3: got $2, want $1 ($(head -c 200 "$W/out"))"
}
iso() { date -u -d "$1" +%Y-%m-%dT%H:%M:%SZ; }
secret() { # <id> <secret> <expiration>
  api "$ADMIN" POST /api/v1/namespaces/kube-system/secrets "{\"apiVersion\":\"v1\",\"kind\":\"Secret\",
    \"metadata\":{\"name\":\"bootstrap-token-$1\"},\"type\":\"bootstrap.kubernetes.io/token\",
    \"stringData\":{\"token-id\":\"$1\",\"token-secret\":\"$2\",\"usage-bootstrap-authentication\":\"true\",
    \"expiration\":\"$3\"}}"
}
openssl req -newkey rsa:2048 -nodes -keyout "$W/n.key" -subj "/O=system:nodes/CN=system:node:n1" 2>/dev/null \
  | base64 -w0 >"$W/req.b64"
csr() { # <name> <signer> — a CSR body claiming to be the victim node
  printf '{"apiVersion":"certificates.k8s.io/v1","kind":"CertificateSigningRequest","metadata":{"name":"%s"},
    "spec":{"signerName":"%s","request":"%s","usages":["client auth"],"username":"system:node:victim","groups":["system:nodes"]}}' \
    "$1" "$2" "$(cat "$W/req.b64")"
}

expect 201 "$(secret abcdef 0123456789abcdef "$(iso '+1 hour')")" "setup: bootstrap token Secret"
expect 201 "$(secret zzzzzz 0123456789abcdef "$(iso '-1 minute')")" "setup: expired bootstrap token Secret"
BOOT=abcdef.0123456789abcdef

# 1.
expect 201 "$(api $BOOT POST $CSRS "$(csr forge-1 storm.io/forge-node)")" "bootstrap token creates a forge-node CSR"
expect system:bootstrap:abcdef "$(field "['spec']['username']")" "spec.username is the token's, not the body's"
expect "['system:bootstrappers', 'system:authenticated']" "$(field "['spec']['groups']")" "spec.groups are the token's"
for s in kubernetes.io/kube-apiserver-client-kubelet kubernetes.io/kube-apiserver-client storm.io/other; do
  expect 403 "$(api $BOOT POST $CSRS "$(csr "other-$RANDOM" $s)")" "bootstrap token refused signerName $s"
done

# 2.
expect 201 "$(api "$ADMIN" POST $CSRS "$(csr admins storm.io/forge-node)")" "setup: an admin's CSR"
expect 200 "$(api $BOOT GET $CSRS/forge-1)" "bootstrap token reads its own CSR"
expect 403 "$(api $BOOT GET $CSRS/admins)" "bootstrap token cannot read another's CSR"
expect 403 "$(api $BOOT GET $CSRS)" "bootstrap token cannot list CSRs"
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 --cacert "$W/ca.crt" -H "Authorization: Bearer $BOOT" \
  "$API$CSRS?watch=true&fieldSelector=metadata.name%3Dforge-1")
expect 200 "$code" "bootstrap token watches its own CSR by name"
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 --cacert "$W/ca.crt" -H "Authorization: Bearer $BOOT" \
  "$API$CSRS?watch=true&fieldSelector=metadata.name%3Dadmins")
expect 403 "$code" "bootstrap token cannot watch another's CSR"

# 3.
expect 401 "$(api zzzzzz.0123456789abcdef GET $CSRS/forge-1)" "expired bootstrap token refused"
expect 401 "$(api abcdef.0123456789abcdee GET $CSRS/forge-1)" "wrong secret refused"
expect 201 "$(secret qqqqqq 0123456789abcdef "$(iso '+1 hour')")" "setup: a third token"
expect 201 "$(api qqqqqq.0123456789abcdef POST $CSRS "$(csr q-1 storm.io/forge-node)")" "a third token authenticates"
expect 200 "$(api "$ADMIN" DELETE /api/v1/namespaces/kube-system/secrets/bootstrap-token-qqqqqq)" "setup: its Secret deleted"
sleep 1
expect 401 "$(api qqqqqq.0123456789abcdef GET $CSRS/forge-1)" "deleted Secret: token refused"

# 4.
NODE=$(token system:node:n1 '["system:nodes"]')
expect 201 "$(api "$NODE" POST $CSRS "$(csr renew-1 storm.io/forge-node)")" "system:node:n1 creates a forge-node CSR"
expect system:node:n1 "$(field "['spec']['username']")" "renewal stamped system:node:n1"

# 5. A user who may write the subresources but holds no signer verbs.
api "$ADMIN" POST /apis/rbac.authorization.k8s.io/v1/clusterroles '{"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole",
  "metadata":{"name":"csr-writer"},"rules":[{"apiGroups":["certificates.k8s.io"],"resources":["certificatesigningrequests",
  "certificatesigningrequests/approval","certificatesigningrequests/status"],"verbs":["get","list","update","patch"]}]}' >/dev/null
bind() { # <binding> <user> <clusterrole>
  api "$ADMIN" POST /apis/rbac.authorization.k8s.io/v1/clusterrolebindings "{\"apiVersion\":\"rbac.authorization.k8s.io/v1\",
    \"kind\":\"ClusterRoleBinding\",\"metadata\":{\"name\":\"$1\"},\"roleRef\":{\"apiGroup\":\"rbac.authorization.k8s.io\",
    \"kind\":\"ClusterRole\",\"name\":\"$3\"},\"subjects\":[{\"kind\":\"User\",\"name\":\"$2\",\"apiGroup\":\"rbac.authorization.k8s.io\"}]}"
}
expect 201 "$(bind csr-writer writer csr-writer)" "setup: writer bound"
expect 201 "$(bind forge-signer stormcert system:storm:forge-node-signer)" "setup: stormcert bound to system:storm:forge-node-signer"
WRITER=$(token writer '[]'); SIGNER=$(token stormcert '[]')
approve() { # <token> <csr> — prints the code
  api "$ADMIN" GET $CSRS/$2 >/dev/null
  python3 -c "import json; d=json.load(open('$W/out')); d.setdefault('status',{})['conditions']=[{'type':'Approved','status':'True','reason':'Test','message':'rig'}]; print(json.dumps(d))" >"$W/appr.json"
  api "$1" PUT $CSRS/$2/approval "$(cat "$W/appr.json")"
}
expect 403 "$(approve "$WRITER" forge-1)" "no 'approve' on the signer: approval refused"
sleep 1
expect 200 "$(approve "$SIGNER" forge-1)" "forge-node signer approves a forge-node CSR"
api "$ADMIN" GET $CSRS/forge-1 >/dev/null
expect True "$(field "['status']['conditions'][0]['status']")" "approval stored"
python3 -c "import json; d=json.load(open('$W/out')); d['status']['certificate']='LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCg=='; print(json.dumps(d))" >"$W/sign.json"
expect 403 "$(api "$WRITER" PUT $CSRS/forge-1/status "$(cat "$W/sign.json")")" "no 'sign' on the signer: certificate refused"
expect 200 "$(api "$SIGNER" PUT $CSRS/forge-1/status "$(cat "$W/sign.json")")" "forge-node signer writes the certificate"
expect 201 "$(api "$ADMIN" POST $CSRS "$(csr kubelet-1 kubernetes.io/kube-apiserver-client-kubelet)")" "setup: a kubelet CSR"
expect 403 "$(approve "$SIGNER" kubelet-1)" "forge-node signer cannot approve a kubelet CSR"

# 6.
api "$ADMIN" POST /apis/authentication.k8s.io/v1/tokenreviews \
  "{\"apiVersion\":\"authentication.k8s.io/v1\",\"kind\":\"TokenReview\",\"spec\":{\"token\":\"$BOOT\"}}" >/dev/null
expect system:bootstrap:abcdef "$(field "['status']['user']['username']")" "TokenReview answers a bootstrap token"

report
