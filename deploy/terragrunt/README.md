# rustkube infrastructure — Terragrunt

Provisions a **dedicated** rustkube stack on the g8 Proxmox using the shared,
versioned module at [`github.com/glennswest/terraform-modules`](https://github.com/glennswest/terraform-modules)
(`modules/proxmox-fedora-vm`, pinned `?ref=`). Same convention as
[`../../../irondirectory/deploy/terragrunt`](https://github.com/glennswest/irondirectory) —
we never copy `.tf`; units reference the module by tag.

```
deploy/terragrunt/
  root.hcl            # provider + token wiring (PROXMOX_API_TOKEN from env), once
  fastetcd/           # unit: rk-etcd1.g8.lo — rustkube's DEDICATED datastore
    terragrunt.hcl
    templates/fastetcd-user-data.yaml.tftpl
  rustkube/           # unit: rustkube1.g8.lo — control plane, --etcd-servers → rk-etcd1
    terragrunt.hcl
    templates/rustkube-user-data.yaml.tftpl
```

## Dedicated, not shared

`fastetcd/` stands up a **separate** single-node fastetcd (`rk-etcd1`,
`192.168.8.51:2379`) for rustkube's `/registry/...` keyspace. It is **not**
irondirectory's `dm1/dm2/dm3` cluster (`192.168.8.41-43`) — that one backs the
AD directory and must not be shared.

## Prerequisites

- `terraform` + `terragrunt` installed (`brew install hashicorp/tap/terraform terragrunt`).
- Proxmox token in `.env` at this repo root (gitignored, chmod 600) — the
  dedicated, pool-scoped `terraform-svc@pve` token. **Never `root@pam`.** See
  irondirectory's README + terraform-modules CLAUDE.md "Incident: 2026-07-08".
  ```sh
  export PROXMOX_API_TOKEN='terraform-svc@pve!rustkube=...'
  ```
- `terraform-modules` reachable (the module `source` clones it over SSH).

## ⚠ Before apply — confirm the vm_ids are free

The `vm_id`s in both units (`2010`, `2011`) are **placeholders** in the allowed
range (2000-2100), with IPs outside the g8 DHCP pool (`.100-.200`). A colliding
`vm_id` is exactly how the 2026-07-08 incident let `terraform destroy` delete an
unrelated VM. Verify with terraform-modules' canonical script:

```sh
../../../terraform-modules/examples/terragrunt/get-free-vmid.sh   # pick free ids, update both terragrunt.hcl
```

## Apply — fastetcd first, then rustkube

```sh
source .env

# 1) Dedicated datastore
cd deploy/terragrunt/fastetcd && terragrunt apply
# rk-etcd1 boots, dnf-installs the pinned fastetcd RPM, serves :2379.

# 2) Control plane (builds rustkube from source in cloud-init)
cd ../rustkube && terragrunt apply
```

## Verify

```sh
# datastore up (plaintext):
etcdctl --endpoints=http://192.168.8.51:2379 endpoint health

# apiserver up (control-plane-only bring-up):
curl -s http://192.168.8.52:6443/healthz && echo   # TLS listener not wired yet; plain HTTP on :6443

# rustkube wrote its bootstrap objects into the dedicated fastetcd:
etcdctl --endpoints=http://192.168.8.51:2379 get --prefix /registry/namespaces
```

## Notes / follow-ups

- **No rustkube RPM yet.** The `rustkube/` node builds from source in cloud-init
  (installs rust, clones, `cargo build --release`). The production path is to
  ship a `rustkube` RPM and `dnf install` it like the fastetcd/iron-ldapd units.
- **Plaintext.** Both fastetcd and the apiserver→fastetcd hop run without TLS
  for first bring-up. To harden: enable `--client-cert-auth` on fastetcd and set
  `ETCD_CACERT/ETCD_CERT/ETCD_KEY` in `/etc/rustkube/rustkube.conf`.
- **Single-node store.** Add `rk-etcd2/3` to the `fastetcd/` unit's `nodes` for a
  3-node Raft cluster (mirror irondirectory's etcd unit).
- **Control-plane only.** `RUSTKUBE_ARGS=--no-kubelet --no-proxy --no-dns` avoids
  needing a container runtime on the node; drop them to run all-in-one.
