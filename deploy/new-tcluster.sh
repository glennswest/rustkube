#!/usr/bin/env bash
#
# Create (or refresh) a NAMED single-node rustkube test cluster.
#
#   ./new-tcluster.sh <name> [ip]        e.g. ./new-tcluster.sh tcluster1 192.168.8.61
#   ./new-tcluster.sh <name> --with-node  also run the kubelet, so pods can RUN
#   ./new-tcluster.sh <name> --destroy
#
# Each cluster is one VM running a 1-member fastetcd plus the whole control
# plane, with its OWN CA — so clusters are fully independent and one can be
# handed to a project/person without giving access to any other.
#
# Produces:
#   deploy/terragrunt/tclusters/<name>/terragrunt.hcl   the unit (own tfstate)
#   deploy/terragrunt/tclusters/<name>/pki/             per-cluster PKI (gitignored)
#   deploy/terragrunt/tclusters/<name>/<name>.kubeconfig  hand this to the user
#
# Idempotent: re-running reuses the existing PKI and VMID and re-applies.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
TCROOT="$HERE/terragrunt/tclusters"
NAME="${1:-}"
ARG2=""
WITH_NODE=0
for a in "${@:2}"; do
  case "$a" in
    --with-node) WITH_NODE=1 ;;
    *)           ARG2="$a" ;;
  esac
done

[ -n "$NAME" ] || { echo "usage: $0 <name> [ip|--destroy]" >&2; exit 1; }
[[ "$NAME" =~ ^[a-z][a-z0-9-]{0,30}$ ]] || {
  echo "name must be a DNS label (lowercase alnum + dashes): $NAME" >&2; exit 1; }

DIR="$TCROOT/$NAME"

# --- destroy -----------------------------------------------------------------
if [ "$ARG2" = "--destroy" ]; then
  [ -d "$DIR" ] || { echo "no such cluster: $NAME" >&2; exit 1; }
  ( cd "$DIR" && terragrunt destroy -auto-approve )
  # Release the VMID reservation so the allocator can hand it out again.
  vmid="$(grep -oE 'vm_id *= *[0-9]+' "$DIR/terragrunt.hcl" | grep -oE '[0-9]+' || true)"
  [ -n "$vmid" ] && bash "$HERE/terragrunt/free-vmid.sh" --release "$vmid" || true
  echo "destroyed $NAME (kept $DIR/pki — delete it by hand to rotate the CA)"
  exit 0
fi

# --- identity: reuse on re-run, else allocate --------------------------------
mkdir -p "$DIR/pki"
if [ -f "$DIR/terragrunt.hcl" ]; then
  IP="$(grep -oE 'ip *= *"[0-9.]+"' "$DIR/terragrunt.hcl" | grep -oE '[0-9.]+')"
  VMID="$(grep -oE 'vm_id *= *[0-9]+' "$DIR/terragrunt.hcl" | grep -oE '[0-9]+')"
  echo "reusing existing $NAME -> ip=$IP vm_id=$VMID"
else
  IP="${ARG2:-}"
  if [ -z "$IP" ]; then
    # First free address in the tcluster range (.61-.79), outside the g8 DHCP
    # pool (.100-.200) and clear of the masters (.51-.53).
    for o in $(seq 61 79); do
      cand="192.168.8.$o"
      grep -rqs "\"$cand\"" "$TCROOT"/*/terragrunt.hcl 2>/dev/null && continue
      ping -c1 -W1 "$cand" >/dev/null 2>&1 && continue   # answered = in use
      IP="$cand"; break
    done
  fi
  [ -n "$IP" ] || { echo "no free IP in 192.168.8.61-79" >&2; exit 1; }
  # Reject an address already claimed by another unit (the .95 duplication bug).
  if grep -rqs "\"$IP\"" "$TCROOT"/*/terragrunt.hcl 2>/dev/null; then
    echo "IP $IP is already claimed by another tcluster" >&2; exit 1
  fi
  VMID="$(bash "$HERE/terragrunt/free-vmid.sh")"
  echo "allocated $NAME -> ip=$IP vm_id=$VMID"
fi

# MAC is derived from the last octet so it is stable across rebuilds and unique.
LAST_OCTET="${IP##*.}"
MAC="$(printf 'BC:24:11:08:01:%02X' "$LAST_OCTET")"

# --- per-cluster PKI (own CA) ------------------------------------------------
DAYS=3650
KUBE_SVC_IP=10.96.0.1
( cd "$DIR/pki"
  have() { [ -s "$1" ]; }
  if ! have ca.crt; then
    openssl genrsa -out ca.key 2048 2>/dev/null
    openssl req -x509 -new -nodes -key ca.key -subj "/CN=$NAME-ca" -days "$DAYS" -out ca.crt 2>/dev/null
  fi
  if ! have sa.key; then
    openssl genrsa -out sa.key 2048 2>/dev/null
    openssl rsa -in sa.key -pubout -out sa.pub 2>/dev/null
  fi
  gen_client() {
    local base="$1" cn="$2" org="${3:-}"
    have "$base.crt" && return 0
    local subj="/CN=$cn"; [ -n "$org" ] && subj="/CN=$cn/O=$org"
    openssl genrsa -out "$base.key" 2048 2>/dev/null
    openssl req -new -key "$base.key" -subj "$subj" -out "$base.csr" 2>/dev/null
    openssl x509 -req -in "$base.csr" -CA ca.crt -CAkey ca.key -CAcreateserial -days "$DAYS" \
      -extfile <(printf "extendedKeyUsage=clientAuth") -out "$base.crt" 2>/dev/null
    rm -f "$base.csr"
  }
  gen_client admin              admin                            system:masters
  gen_client controller-manager system:kube-controller-manager
  gen_client scheduler          system:kube-scheduler

  if ! have apiserver.crt; then
    openssl genrsa -out apiserver.key 2048 2>/dev/null
    openssl req -new -key apiserver.key -subj "/CN=kube-apiserver" -out apiserver.csr 2>/dev/null
    openssl x509 -req -in apiserver.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days "$DAYS" \
      -extfile <(cat <<EOF
subjectAltName=DNS:kubernetes,DNS:kubernetes.default,DNS:kubernetes.default.svc,DNS:kubernetes.default.svc.cluster.local,DNS:$NAME.g8.lo,DNS:localhost,IP:127.0.0.1,IP:$IP,IP:$KUBE_SVC_IP
extendedKeyUsage=serverAuth
EOF
) -out apiserver.crt 2>/dev/null
    rm -f apiserver.csr
  fi
)
echo "PKI ready ($DIR/pki, CN=$NAME-ca)"

# --- node agent (optional): long-lived token signed by this cluster's SA key ---
RUSTKUBE_NODE_RPM="https://github.com/glennswest/rustkube-node/releases/download/v0.1.0/rustkube-node-0.1.0-1.fc43.x86_64.rpm"
if [ "$WITH_NODE" = "1" ]; then
  if [ ! -s "$DIR/pki/node-token" ]; then
    bash "$HERE/gen-node-token.sh" "$DIR/pki/sa.key" "$NAME" > "$DIR/pki/node-token"
    chmod 600 "$DIR/pki/node-token"
  fi
  echo "node token ready (sub=system:node:$NAME)"
fi

# --- the terragrunt unit -----------------------------------------------------
# Identity only; all shared logic lives in ../templates/tcluster-user-data.yaml.tftpl
# and the pinned RPM URLs below (keep in sync with the masters unit).
RUSTKUBE_RPM="$(grep -oE 'https://[^"]*kubernetes-rs-[0-9.]+-1\.x86_64\.rpm' \
  "$HERE/terragrunt/masters/terragrunt.hcl" | head -1)"
FASTETCD_RPM="$(grep -oE 'https://[^"]*fastetcd-[0-9.]+-1\.x86_64\.rpm' \
  "$HERE/terragrunt/masters/terragrunt.hcl" | head -1)"

cat > "$DIR/terragrunt.hcl" <<HCL
# Single-node rustkube test cluster "$NAME" — generated by deploy/new-tcluster.sh.
# Edit that script (or the shared template) rather than this file; re-running the
# script regenerates it and reuses this cluster's IP, VMID and PKI.

include "root" {
  path = find_in_parent_folders("root.hcl")
}

terraform {
  source = "git::ssh://git@github.com/glennswest/terraform-modules.git//modules/proxmox-fedora-vm?ref=v0.3.0"
}

locals {
  name          = "$NAME"
  ip            = "$IP"
  vm_id         = $VMID
  mac           = "$MAC"
  cluster_token = "$NAME-etcd"
  ssh_key       = trimspace(file(pathexpand("~/.ssh/id_rsa.pub")))
  pki           = "\${get_terragrunt_dir()}/pki"

  rustkube_rpm_url = "$RUSTKUBE_RPM"
  fastetcd_rpm_url = "$FASTETCD_RPM"

  # Node agent: when true the VM also runs the kubelet, so pods actually run.
  with_node             = $([ "$WITH_NODE" = "1" ] && echo true || echo false)
  rustkube_node_rpm_url = "$RUSTKUBE_NODE_RPM"
}

inputs = {
  dns_zone_id        = "9bed60c8-1664-4183-88f9-a1a21b927edc" # g8.lo
  ci_ssh_public_keys = [local.ssh_key]
  tags               = ["terraform", "fedora", "rustkube", "tcluster"]

  vm_datastore      = "test-lvm-thin"
  snippet_datastore = "terraform-snippets"

  vms = {
    (local.name) = {
      vm_id     = local.vm_id
      mac       = local.mac
      ip        = local.ip
      cores     = 2
      memory    = 2048
      disk_size = 20
      user_data = templatefile("\${get_terragrunt_dir()}/../templates/tcluster-user-data.yaml.tftpl", {
        hostname         = local.name
        fqdn             = "\${local.name}.g8.lo"
        ci_user          = "fedora"
        ssh_keys         = [local.ssh_key]
        node_ip          = local.ip
        cluster_token    = local.cluster_token
        fastetcd_rpm_url = local.fastetcd_rpm_url
        rustkube_rpm_url = local.rustkube_rpm_url
        with_node             = local.with_node
        rustkube_node_rpm_url = local.rustkube_node_rpm_url
        node_token            = local.with_node ? trimspace(file("\${local.pki}/node-token")) : ""
        ca_crt        = file("\${local.pki}/ca.crt")
        ca_key        = file("\${local.pki}/ca.key")
        sa_key        = file("\${local.pki}/sa.key")
        sa_pub        = file("\${local.pki}/sa.pub")
        apiserver_crt = file("\${local.pki}/apiserver.crt")
        apiserver_key = file("\${local.pki}/apiserver.key")
        cm_crt        = file("\${local.pki}/controller-manager.crt")
        cm_key        = file("\${local.pki}/controller-manager.key")
        sched_crt     = file("\${local.pki}/scheduler.crt")
        sched_key     = file("\${local.pki}/scheduler.key")
        admin_crt     = file("\${local.pki}/admin.crt")
        admin_key     = file("\${local.pki}/admin.key")
      })
    }
  }
}
HCL
echo "wrote $DIR/terragrunt.hcl"

# --- kubeconfig to hand out ---------------------------------------------------
KCFG="$DIR/$NAME.kubeconfig"
b64() { base64 < "$1" | tr -d '\n'; }
cat > "$KCFG" <<EOF
apiVersion: v1
kind: Config
current-context: $NAME
clusters:
- name: $NAME
  cluster:
    server: https://$IP:6443
    certificate-authority-data: $(b64 "$DIR/pki/ca.crt")
users:
- name: $NAME-admin
  user:
    client-certificate-data: $(b64 "$DIR/pki/admin.crt")
    client-key-data: $(b64 "$DIR/pki/admin.key")
contexts:
- name: $NAME
  context:
    cluster: $NAME
    user: $NAME-admin
EOF
chmod 600 "$KCFG"
echo "wrote $KCFG"

# --- apply --------------------------------------------------------------------
if [ "${TCLUSTER_NO_APPLY:-}" = "1" ]; then
  echo "TCLUSTER_NO_APPLY=1 set — skipping apply. Run: (cd $DIR && terragrunt apply)"
  exit 0
fi
( cd "$DIR" && terragrunt apply -auto-approve )

echo
echo "cluster $NAME is up at https://$IP:6443"
echo "  KUBECONFIG=$KCFG kubectl get namespaces"
