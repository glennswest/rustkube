# rustkube infrastructure — Terragrunt

The guide is [`docs/terragrunt-deploy.md`](../../docs/terragrunt-deploy.md):
the 3-master `masters/` unit, the `tclusters/` test clusters, PKI, and
operating runbooks. Read its warning first — as of 2026-09-24 a fresh
provision cannot install a current rustkube, because no release since v0.7.30
carries the RPM cloud-init installs.

This path provisions Proxmox VMs. It is not how stormcos runs rustkube.

The single-node `fastetcd/` + `rustkube/` units this README used to describe
(rk-etcd1, rustkube1, VMIDs 2010/2011) no longer exist; their IPs belong to
master1 and master2. VMIDs come from `free-vmid.sh` (range 2000–2100).
