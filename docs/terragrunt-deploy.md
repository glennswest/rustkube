# Deploying the rustkube masters with Terragrunt

How to bring up (and safely operate) the 3-master control plane on Proxmox with
Terragrunt. Everything installs **released RPMs** via cloud-init — no building on
the nodes.

Unit: `deploy/terragrunt/masters/` → `master1/2/3.g8.lo` (192.168.8.51/.52/.53),
each running a fastetcd raft member + kube-apiserver + kube-controller-manager +
kube-scheduler.

---

## Prerequisites

- `terragrunt` + `terraform` (or `tofu`), `jq`, `openssl`, `curl`, `python3`.
- **Proxmox API token** for the `terraform-svc@pve` service account, in the
  environment (never in code/state):
  ```bash
  export PROXMOX_API_TOKEN='terraform-svc@pve!<tokenid>=<uuid>'
  ```
- SSH public key at `~/.ssh/id_rsa.pub` (injected for the `fedora` user).
- A published rustkube RPM release; the pin lives in `terragrunt.hcl`
  (`rustkube_rpm_url`). Bump it there when releasing a new version.

## 1. Generate the PKI (once)

```bash
./deploy/gen-pki.sh          # → deploy/terragrunt/masters/pki/ (gitignored)
```

Creates the cluster CA, SA signing key, per-master serving certs (with SANs),
component/admin client certs, and the node bootstrap cert. Idempotent — reruns
keep the existing CA. Terragrunt injects these via cloud-init.

## 2. Deploy

```bash
cd deploy/terragrunt/masters
terragrunt init
terragrunt plan       # review: expect 9 to add on a clean cluster
terragrunt apply
```

All 3 masters boot together, install the fastetcd + kubernetes-rs RPMs, form the
raft cluster, and start the TLS control plane.

## 3. Verify

```bash
./deploy/verify-cluster.sh plaintext   # (or run the HTTPS checks below)
```

Or directly, using the admin cert:

```bash
cd deploy/terragrunt/masters/pki
for ip in 192.168.8.51 192.168.8.52 192.168.8.53; do
  curl -s --cacert ca.crt --cert admin.crt --key admin.key https://$ip:6443/healthz
done
```

Expect `ok` on all three, `anonymous → 401`, and namespace writes replicated
across masters.

---

## Operating: upgrades and replacement

### Version upgrade (preferred) — in-place, no data loss

For a new rustkube version, **upgrade in place** — do NOT recreate VMs:

```bash
RPM="https://github.com/glennswest/rustkube/releases/download/vX.Y.Z/kubernetes-rs-X.Y.Z-1.x86_64.rpm"
for ip in 192.168.8.51 192.168.8.52 192.168.8.53; do
  ssh fedora@$ip "sudo dnf install -y $RPM && \
    sudo systemctl restart kube-apiserver kube-controller-manager kube-scheduler"
done
```

Then bump `rustkube_rpm_url` in `terragrunt.hcl` so future provisions match.
This keeps fastetcd (and all cluster data) untouched.

### Full redeploy (destroys all data)

```bash
terragrunt destroy -auto-approve
terragrunt apply   -auto-approve
```

Recreates all 3 masters from scratch — **fastetcd starts empty**. Only for a
clean rebuild.

---

## ⚠️ Gotchas (hard-won)

1. **Never `rm -rf .terragrunt-cache`.** The local tfstate lives under it. Delete
   it and Terragrunt loses track of the running VMs; the next `destroy` finds
   nothing and the next `apply` re-adopts or duplicates. To pick up edited
   templates, just re-run `terragrunt init` (it re-copies the unit).

2. **Do NOT replace a single master with `-target destroy` + full `apply`.**
   A partial `-target destroy` of one VM followed by a non-targeted `apply`
   **cascades into replacing all three masters** (observed: `6 added, 5 destroyed`),
   which wipes fastetcd and loses all cluster data. Verified via the HA soak test.
   - For a version change → use the **in-place upgrade** above.
   - True single-member replacement needs a fastetcd member remove/add-as-existing
     procedure (the static `ETCD_INITIAL_CLUSTER_STATE=new` bootstrap does not
     safely re-add one member) — tracked as a hardening item.

3. **Master *loss* is safe; master *recreate* is not (yet).** Losing one master
   (VM stop/destroy) leaves the cluster fully serving on the other two (raft
   quorum 2/3, data intact). It's the *recreate* path that currently risks the
   cascade above.

4. **Host keys change on every recreate.** Use
   `-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null` for SSH to
   freshly-provisioned masters, or clear stale keys with `ssh-keygen -R <ip>`.

5. **`gen-pki.sh` must run before apply** — the template reads the PKI files via
   `file()`. Missing files → a plan/apply error.

---

## Scale + HA soak test

```bash
./deploy/ha-soak-test.sh scale-only   # 1000 namespaces: create/delete/recreate, verify replication
./deploy/ha-soak-test.sh              # + kill a master, verify survival, recreate, verify
```

Phase 1 (scale) and Phase 2a (master loss → cluster survives with data intact)
pass. Phase 2b (recreate) currently exposes gotcha #2 above.
