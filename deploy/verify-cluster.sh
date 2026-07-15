#!/usr/bin/env bash
# Verify the rustkube masters cluster after `terragrunt apply` (masters unit).
#
# Usage:
#   ./verify-cluster.sh plaintext   # verify the HTTP control plane + 3-node fastetcd
#   ./verify-cluster.sh tls         # enable --tls + x509 on master1, then test HTTPS
#
# Masters: master1/2/3.g8.lo = 192.168.8.51/.52/.53
set -uo pipefail

M1=192.168.8.51 M2=192.168.8.52 M3=192.168.8.53
MODE="${1:-plaintext}"

wait_for() { # url, tries
  for _ in $(seq 1 "${2:-60}"); do curl -sf "$1" >/dev/null 2>&1 && return 0; sleep 2; done; return 1
}

if [ "$MODE" = "plaintext" ]; then
  echo "=== fastetcd 3-node health ==="
  for ip in $M1 $M2 $M3; do
    echo -n "  $ip:2379 → "
    ssh -o ConnectTimeout=8 fedora@$ip "sudo fastetcd-ctl --endpoints=http://127.0.0.1:2379 endpoint health 2>/dev/null || echo unreachable" 2>/dev/null | tail -1
  done
  echo "=== kube-apiserver on master1 ==="
  wait_for "http://$M1:6443/healthz" 90 && echo "  healthz: $(curl -s http://$M1:6443/healthz)" || { echo "  apiserver not up (cloud-init may still be building)"; exit 1; }
  echo "  version: $(curl -s http://$M1:6443/version)"
  echo "=== create a namespace, confirm it persists in fastetcd ==="
  curl -s -XPOST http://$M1:6443/api/v1/namespaces -H 'Content-Type: application/json' \
    -d '{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"cluster-smoke"}}' >/dev/null
  echo "  GET via master2 (different apiserver, shared fastetcd): $(curl -s http://$M2:6443/api/v1/namespaces/cluster-smoke | python3 -c 'import sys,json;print(json.load(sys.stdin)["metadata"]["name"])' 2>/dev/null)"
  echo "  key in fastetcd: $(ssh -o ConnectTimeout=8 fedora@$M1 "sudo fastetcd-ctl --endpoints=http://127.0.0.1:2379 get --prefix /registry/namespaces/cluster-smoke" 2>/dev/null | grep -a /registry | head -1)"
  echo "=== control-plane services ==="
  ssh -o ConnectTimeout=8 fedora@$M1 "systemctl is-active kube-apiserver kube-controller-manager kube-scheduler fastetcd 2>/dev/null" 2>/dev/null | paste -sd' ' -

elif [ "$MODE" = "tls" ]; then
  echo "=== generate a CA + admin client cert (system:masters) locally ==="
  T=$(mktemp -d)
  openssl genrsa -out "$T/ca.key" 2048 2>/dev/null
  openssl req -x509 -new -nodes -key "$T/ca.key" -subj "/CN=rustkube-ca" -days 3650 -out "$T/ca.pem" 2>/dev/null
  openssl genrsa -out "$T/admin.key" 2048 2>/dev/null
  openssl req -new -key "$T/admin.key" -subj "/CN=admin/O=system:masters" -out "$T/admin.csr" 2>/dev/null
  openssl x509 -req -in "$T/admin.csr" -CA "$T/ca.pem" -CAkey "$T/ca.key" -CAcreateserial -days 365 \
    -out "$T/admin.pem" -extfile <(printf "extendedKeyUsage=clientAuth") 2>/dev/null
  echo "=== push CA to master1 + enable --tls --client-ca-file, restart apiserver ==="
  scp -o ConnectTimeout=8 "$T/ca.pem" fedora@$M1:/tmp/client-ca.pem >/dev/null 2>&1
  ssh -o ConnectTimeout=8 fedora@$M1 '
    sudo install -D -m0644 /tmp/client-ca.pem /etc/kubernetes/pki/client-ca.pem
    echo "KUBE_APISERVER_ARGS=--tls --client-ca-file /etc/kubernetes/pki/client-ca.pem" | sudo tee -a /etc/kubernetes/kube-apiserver >/dev/null
    sudo systemctl restart kube-apiserver' 2>/dev/null
  sleep 4
  echo "=== HTTPS + x509 tests against master1 ==="
  echo -n "  https healthz → "; curl -sk "https://$M1:6443/healthz"; echo
  echo -n "  anonymous list namespaces → "; curl -sk -o /dev/null -w "%{http_code}\n" "https://$M1:6443/api/v1/namespaces"
  echo -n "  x509 admin (system:masters) list namespaces → "; curl -sk --cert "$T/admin.pem" --key "$T/admin.key" -o /dev/null -w "%{http_code}\n" "https://$M1:6443/api/v1/namespaces"
  rm -rf "$T"
else
  echo "usage: $0 {plaintext|tls}" >&2; exit 2
fi
