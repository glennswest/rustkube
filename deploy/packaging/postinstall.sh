#!/bin/sh
# Post-install for the kubernetes-rs package. Creates the data dir and reloads
# systemd. Services are NOT auto-enabled — the operator enables what the node
# runs (e.g. `systemctl enable --now kube-apiserver` after setting ETCD_SERVERS
# in /etc/kubernetes/kube-apiserver).
set -e

mkdir -p /var/lib/kubernetes /etc/kubernetes/pki

if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
fi

echo "kubernetes-rs installed. Next:"
echo "  1) set ETCD_SERVERS in /etc/kubernetes/kube-apiserver (your fastetcd)"
echo "  2) systemctl enable --now kube-apiserver kube-controller-manager kube-scheduler"
