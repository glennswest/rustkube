#!/usr/bin/env bash
#
# Generate the rustkube control-plane PKI (OpenShift/kubeadm style) for the
# masters, into an output dir that terragrunt injects via cloud-init write_files.
# Idempotent: existing files are kept, so re-running an apply reuses the CA.
#
#   ./gen-pki.sh [OUTDIR]   (default: deploy/terragrunt/masters/pki)
#
# Produces:
#   ca.crt/ca.key                       cluster CA (kubernetes-ca)
#   sa.key/sa.pub                       service-account token signing keypair
#   apiserver-<m>.crt/.key              per-master serving cert (SANs per node)
#   admin.crt/.key                      CN=admin,   O=system:masters (kubectl)
#   controller-manager.crt/.key         CN=system:kube-controller-manager
#   scheduler.crt/.key                  CN=system:kube-scheduler
#
set -euo pipefail

OUT="${1:-$(cd "$(dirname "$0")" && pwd)/terragrunt/masters/pki}"
mkdir -p "$OUT"; cd "$OUT"

# Masters (keep in sync with terragrunt masters `nodes`).
declare -A MASTERS=( [master1]=192.168.8.51 [master2]=192.168.8.52 [master3]=192.168.8.53 )
KUBE_SVC_IP=10.96.0.1   # apiserver ClusterIP (first IP of the service CIDR)
DAYS=3650

have() { [ -s "$1" ]; }

# --- cluster CA ---
if ! have ca.crt; then
  openssl genrsa -out ca.key 2048
  openssl req -x509 -new -nodes -key ca.key -subj "/CN=kubernetes-ca" -days "$DAYS" -out ca.crt
  echo "generated CA"
fi

# --- service-account signing keypair ---
if ! have sa.key; then
  openssl genrsa -out sa.key 2048
  openssl rsa -in sa.key -pubout -out sa.pub
  echo "generated SA signing keypair"
fi

# Sign a client cert: $1=basename $2=CN $3=O(optional)
gen_client() {
  local base="$1" cn="$2" org="${3:-}"
  have "$base.crt" && return 0
  local subj="/CN=$cn"; [ -n "$org" ] && subj="/CN=$cn/O=$org"
  openssl genrsa -out "$base.key" 2048
  openssl req -new -key "$base.key" -subj "$subj" -out "$base.csr"
  openssl x509 -req -in "$base.csr" -CA ca.crt -CAkey ca.key -CAcreateserial -days "$DAYS" \
    -extfile <(printf "extendedKeyUsage=clientAuth") -out "$base.crt"
  rm -f "$base.csr"
  echo "generated client cert $base ($subj)"
}

gen_client admin              admin                            system:masters
gen_client controller-manager system:kube-controller-manager
gen_client scheduler          system:kube-scheduler

# --- per-master apiserver serving certs (SANs) ---
for m in "${!MASTERS[@]}"; do
  ip="${MASTERS[$m]}"
  have "apiserver-$m.crt" && continue
  openssl genrsa -out "apiserver-$m.key" 2048
  openssl req -new -key "apiserver-$m.key" -subj "/CN=kube-apiserver" -out "apiserver-$m.csr"
  openssl x509 -req -in "apiserver-$m.csr" -CA ca.crt -CAkey ca.key -CAcreateserial -days "$DAYS" \
    -extfile <(cat <<EOF
subjectAltName=DNS:kubernetes,DNS:kubernetes.default,DNS:kubernetes.default.svc,DNS:kubernetes.default.svc.cluster.local,DNS:$m.g8.lo,DNS:localhost,IP:127.0.0.1,IP:$ip,IP:$KUBE_SVC_IP
extendedKeyUsage=serverAuth
EOF
) -out "apiserver-$m.crt"
  rm -f "apiserver-$m.csr"
  echo "generated apiserver serving cert for $m ($ip)"
done

echo "PKI ready in $OUT"
