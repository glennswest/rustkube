#!/usr/bin/env bash
#
# HA + scale soak test for the rustkube masters (kube + fastetcd resilience).
#
#   Phase 1 (scale): create 1000 namespaces, verify; delete 500, verify;
#                    create 500 more, verify — all via the HTTPS API.
#   Phase 2 (HA):    destroy the current master VM, verify the cluster still
#                    serves (quorum on the other two, data intact); recreate the
#                    master, let it rejoin + sync, verify consistency.
#
# Run from deploy/terragrunt/masters (needs the admin cert in ./pki and
# PROXMOX_API_TOKEN for the VM kill/recreate).
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
MASTERS_DIR="$HERE/terragrunt/masters"
PKI="$MASTERS_DIR/pki"
M1=192.168.8.51 M2=192.168.8.52 M3=192.168.8.53
C=(--cacert "$PKI/ca.crt" --cert "$PKI/admin.crt" --key "$PKI/admin.key" -s --max-time 15)

ns_count() { # via $1
  curl "${C[@]}" "https://$1:6443/api/v1/namespaces?limit=5000" \
    | python3 -c 'import sys,json;print(len(json.load(sys.stdin).get("items",[])))' 2>/dev/null
}
export PKI M1
create_ns() { curl -s --max-time 15 --cacert "$PKI/ca.crt" --cert "$PKI/admin.crt" --key "$PKI/admin.key" \
  -o /dev/null -XPOST -H 'Content-Type: application/json' \
  "https://$M1:6443/api/v1/namespaces" -d "{\"apiVersion\":\"v1\",\"kind\":\"Namespace\",\"metadata\":{\"name\":\"$1\"}}"; }
delete_ns() { curl -s --max-time 15 --cacert "$PKI/ca.crt" --cert "$PKI/admin.crt" --key "$PKI/admin.key" \
  -o /dev/null -XDELETE "https://$M1:6443/api/v1/namespaces/$1"; }
export -f create_ns delete_ns

echo "=== baseline namespaces: $(ns_count $M1) ==="

echo "=== Phase 1a: create 1000 namespaces (soak-0000..0999) ==="
seq -w 0 999 | xargs -P 32 -I{} bash -c 'create_ns soak-{}' >/dev/null 2>&1
echo "  count via master1: $(ns_count $M1)   master2: $(ns_count $M2)   master3: $(ns_count $M3)"

echo "=== Phase 1b: delete 500 (soak-0000..0499), verify ==="
seq -w 0 499 | xargs -P 32 -I{} bash -c 'delete_ns soak-{}' >/dev/null 2>&1
echo "  count: $(ns_count $M1)  (expect baseline+500)"

echo "=== Phase 1c: create 500 more (soak-1000..1499), verify ==="
seq -w 1000 1499 | xargs -P 32 -I{} bash -c 'create_ns soak-{}' >/dev/null 2>&1
echo "  count: $(ns_count $M1)  (expect baseline+1000)"
echo "  cross-check replicated: master2=$(ns_count $M2) master3=$(ns_count $M3)"

if [ "${1:-}" = "scale-only" ]; then echo "done (scale-only)"; exit 0; fi

echo "=== Phase 2a: HA — stop master1, verify cluster survives on master2/3 ==="
TOKEN="${PROXMOX_API_TOKEN:?set PROXMOX_API_TOKEN}"
curl -sk -XPOST "https://pve.g8.lo:8006/api2/json/nodes/pve/qemu/2000/status/stop" \
  -H "Authorization: PVEAPIToken=$TOKEN" -o /dev/null
sleep 12
echo "  master2 healthz: $(curl "${C[@]}" https://$M2:6443/healthz)  namespaces: $(ns_count $M2)"
echo "  master3 healthz: $(curl "${C[@]}" https://$M3:6443/healthz)  namespaces: $(ns_count $M3)"
echo "  (data must be intact — fastetcd quorum holds with 2/3)"

echo "=== Phase 2b: safe replace master1 (member remove/add + -replace, state=existing) ==="
# NOTE: the naive 'destroy -target + full apply' cascades to recreate ALL masters
# and wipes fastetcd (see docs/terragrunt-deploy.md). Use the safe runbook:
( cd "$MASTERS_DIR" && "$HERE/replace-master.sh" master1 )

echo "=== Phase 2c: verify rejoin + consistency (all 3 apiservers should match) ==="
for _ in $(seq 1 40); do curl "${C[@]}" https://$M1:6443/healthz 2>/dev/null | grep -q ok && break; sleep 8; done
echo "  m1=$(ns_count $M1)  m2=$(ns_count $M2)  m3=$(ns_count $M3)"
echo "  (mismatch here = a real bug — the test's job. Known: fastetcd#8 rejoin-resync,"
echo "   rustkube#18 watch-cache staleness.)"
