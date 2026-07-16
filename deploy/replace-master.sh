#!/usr/bin/env bash
#
# Safely replace ONE master (fastetcd member + VM) without disturbing the other
# two — the manual equivalent of OpenShift's cluster-etcd-operator member dance:
#
#   1. etcd member remove <target>      (from a healthy node)
#   2. etcd member add    <target>      (register the fresh member as a learner)
#   3. terragrunt -replace <target> VM  (recreate ONLY that VM, booting fastetcd
#                                        with ETCD_INITIAL_CLUSTER_STATE=existing
#                                        so it rejoins + resyncs from the leader)
#
# Avoids the cascade trap (never `-target destroy` + full `apply`). Uses real
# etcdctl against fastetcd's etcd v3 Cluster API.
#
#   Run from deploy/terragrunt/masters, with PROXMOX_API_TOKEN set and etcdctl
#   on PATH:   ./replace-master.sh master1
#
set -euo pipefail

M="${1:?usage: replace-master.sh <master1|master2|master3>}"
declare -A IP=( [master1]=192.168.8.51 [master2]=192.168.8.52 [master3]=192.168.8.53 )
TARGET_IP="${IP[$M]:?unknown master $M}"
export ETCDCTL_API=3

# Member ops (change_membership) must go to the raft LEADER — fastetcd does not
# forward them (it errors "has to forward request to <leader>"). Find the leader
# among the healthy non-target peers and target it directly.
LEADER=""
for m in master1 master2 master3; do
  [ "$m" = "$M" ] && continue
  ep="http://${IP[$m]}:2379"
  # Column order differs across etcd versions; use JSON: leader == this member.
  isldr=$(etcdctl --endpoints="$ep" endpoint status -w json 2>/dev/null \
    | jq -r 'if .[0].Status.leader == .[0].Status.header.member_id then "true" else "false" end' 2>/dev/null)
  if [ "$isldr" = "true" ]; then LEADER="$ep"; break; fi
done
[ -n "$LEADER" ] || { echo "no healthy leader found among peers"; exit 1; }
echo ">> leader endpoint: $LEADER"
EP="--endpoints=$LEADER"

echo ">> members before:"; etcdctl $EP member list -w table

# 1. Remove the target's old member (if the cluster still lists it).
ID=$(etcdctl $EP member list | awk -F', ' -v n="$M" 'index($3,n){print $1}' | head -1)
if [ -n "$ID" ]; then
  echo ">> member remove $M ($ID)"
  etcdctl $EP member remove "$ID"
fi

# 2. Add the target back as a fresh member (learner → promoted by fastetcd).
echo ">> member add $M peer=http://$TARGET_IP:2380"
etcdctl $EP member add "$M" --peer-urls="http://$TARGET_IP:2380" >/dev/null

# 3. Recreate ONLY the target VM; RK_REPLACE_MASTER makes its cloud-init boot
#    fastetcd with state=existing so it joins instead of bootstrapping anew.
echo ">> terragrunt -replace $M (state=existing)"
RK_REPLACE_MASTER="$M" terragrunt apply -auto-approve -no-color -input=false \
  -replace="proxmox_virtual_environment_vm.vm[\"$M\"]" 2>&1 | grep -E "Apply complete|Error" | tail -1

# 4. Wait for the target to rejoin + become healthy.
echo ">> waiting for $M ($TARGET_IP) to rejoin + resync..."
for _ in $(seq 1 75); do
  etcdctl --endpoints="http://$TARGET_IP:2379" endpoint health >/dev/null 2>&1 && { echo "  $M etcd healthy"; break; }
  sleep 8
done

echo ">> members after:"; etcdctl $EP member list -w table
