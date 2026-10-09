# Terragrunt deployment (retired)

The Terragrunt/Proxmox provisioning path — `deploy/terragrunt/` (masters,
tcluster1–4, tnode1), the `kubernetes-rs` RPM packaging (`deploy/packaging/`,
`deploy/systemd/`) and the scripts that only served it (`new-tcluster.sh`,
`replace-master.sh`, `ha-soak-test.sh`, `verify-cluster.sh`) — was removed on
2026-10-09 (#157, owner's decision).

It could no longer install a current rustkube: cloud-init installed the newest
RPM from the latest GitHub release, and no release since v0.7.30 carried one;
the templates also expected kube-proxy and CRI-O from a rustkube-node RPM,
and kube-proxy is replaced by Cilium.

- **How rustkube ships:** as stormd goldens in stormcos (README, *How it
  ships*; docs/releasing.md).
- **How it is tested:** the component's test image on stormcentral's test
  machines (test/README.md).
- **Standalone PKI:** `deploy/gen-pki.sh`, `deploy/gen-node-token.sh` and
  `deploy/renew-certs.sh` stay (docs/certificates.md).
- **The old runbook and files:** git history, e.g.
  `git show 69417c2:docs/terragrunt-deploy.md` and
  `git ls-tree -r 69417c2 deploy/terragrunt`.
