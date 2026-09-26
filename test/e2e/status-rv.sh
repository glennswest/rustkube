#!/usr/bin/env bash
#
# PUT …/status is conditional on the body's resourceVersion (#78), against a
# real apiserver on a real fastetcd, for each kind of status handler:
# built-in cluster-scoped (Node), built-in namespaced (PVC), a custom
# resource, and the CSR /approval subresource that shares the cluster path.
#
#   test/e2e/status-rv.sh           # from the checkout; builds what it runs
#
# For each: a PUT carrying the current resourceVersion lands; the same PUT
# again, now one version behind, is a 409 and changes nothing; a PUT with no
# resourceVersion lands. Exit status is the number of failed checks.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

req() { # <method> <path> [json] — prints the HTTP code, body in $W/out
  curl -sk -o "$W/out" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $ADMIN" \
    -H 'Content-Type: application/json' "$API$2" ${3:+-d "$3"}
}
rv() { grep -o '"resourceVersion":"[0-9]*"' "$W/out" | head -1 | grep -o '[0-9][0-9]*'; }
field() { grep -o "\"$1\":\"[^\"]*\"" "$W/out" | head -1 | cut -d'"' -f4; }

# <what> <object path> <status path> <status json with a __MARK__ string>
check() {
  local what=$1 obj=$2 st=$3 tmpl=$4 code rv1 rv2
  req GET "$obj" >/dev/null; rv1=$(rv)
  [ -n "$rv1" ] || { fail "$what: no object at $obj: $(cat "$W/out")"; return; }

  body() { # <rv or empty> <mark>
    local meta='{}'; [ -n "$1" ] && meta="{\"resourceVersion\":\"$1\"}"
    printf '{"metadata":%s,"status":%s}' "$meta" "${tmpl//__MARK__/$2}"
  }

  code=$(req PUT "$st" "$(body "$rv1" first)")
  [ "$code" = 200 ] && pass "$what: current resourceVersion lands" \
    || { fail "$what: current RV: $code $(cat "$W/out")"; return; }
  rv2=$(rv)

  code=$(req PUT "$st" "$(body "$rv1" stale)")
  [ "$code" = 409 ] && pass "$what: stale resourceVersion is 409" || fail "$what: stale RV: $code"
  req GET "$obj" >/dev/null
  if grep -q stale "$W/out"; then fail "$what: the stale status was written"
  elif [ "$(rv)" != "$rv2" ]; then fail "$what: object changed ($rv2 -> $(rv))"
  else pass "$what: stale write left the object as it was"; fi

  code=$(req PUT "$st" "$(body "" none)")
  req GET "$obj" >/dev/null
  [ "$code" = 200 ] && grep -q none "$W/out" && pass "$what: no resourceVersion lands" \
    || fail "$what: no RV: $code"
}

# Built-in, cluster-scoped.
req POST /api/v1/nodes '{"apiVersion":"v1","kind":"Node","metadata":{"name":"n1"}}' >/dev/null
check "node" /api/v1/nodes/n1 /api/v1/nodes/n1/status \
  '{"conditions":[{"type":"Ready","status":"True","reason":"__MARK__"}]}'

# Built-in, namespaced.
req POST /api/v1/namespaces/default/persistentvolumeclaims '{"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":"c1"},"spec":{"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":"1Gi"}}}}' >/dev/null
check "pvc" /api/v1/namespaces/default/persistentvolumeclaims/c1 \
  /api/v1/namespaces/default/persistentvolumeclaims/c1/status '{"phase":"__MARK__"}'

# A custom resource with a status subresource.
req POST /apis/apiextensions.k8s.io/v1/customresourcedefinitions '{"apiVersion":"apiextensions.k8s.io/v1","kind":"CustomResourceDefinition","metadata":{"name":"widgets.demo.io"},"spec":{"group":"demo.io","scope":"Namespaced","names":{"plural":"widgets","singular":"widget","kind":"Widget"},"versions":[{"name":"v1","served":true,"storage":true,"subresources":{"status":{}},"schema":{"openAPIV3Schema":{"type":"object","x-kubernetes-preserve-unknown-fields":true}}}]}}' >/dev/null
for _ in $(seq 20); do
  [ "$(req GET /apis/demo.io/v1/namespaces/default/widgets)" = 200 ] && break
  sleep 1
done
req POST /apis/demo.io/v1/namespaces/default/widgets '{"apiVersion":"demo.io/v1","kind":"Widget","metadata":{"name":"w1"},"spec":{"size":1}}' >/dev/null
check "custom resource" /apis/demo.io/v1/namespaces/default/widgets/w1 \
  /apis/demo.io/v1/namespaces/default/widgets/w1/status '{"state":"__MARK__"}'
req GET /apis/demo.io/v1/namespaces/default/widgets/w1 >/dev/null
grep -q '"size":1' "$W/out" && pass "custom resource: spec untouched by status PUTs" \
  || fail "custom resource: spec changed: $(cat "$W/out")"

# The CSR /approval subresource, which shares the cluster-scoped status path.
req POST /apis/certificates.k8s.io/v1/certificatesigningrequests '{"apiVersion":"certificates.k8s.io/v1","kind":"CertificateSigningRequest","metadata":{"name":"csr1"},"spec":{"request":"","signerName":"kubernetes.io/kube-apiserver-client","usages":["client auth"]}}' >/dev/null
check "csr /approval" /apis/certificates.k8s.io/v1/certificatesigningrequests/csr1 \
  /apis/certificates.k8s.io/v1/certificatesigningrequests/csr1/approval \
  '{"conditions":[{"type":"Approved","status":"True","reason":"__MARK__"}]}'

report
