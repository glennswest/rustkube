#!/usr/bin/env bash
#
# Renew the control-plane leaf certificates from the cluster CA, in place.
#
# The apiserver picks up a new serving certificate **without a restart** — it
# resolves the certificate per handshake and watches the file (rustkube#20) —
# so renewing it is writing two files. The client components (controller
# manager, scheduler) build their TLS identity once at startup and do need a
# restart; that is cheap and safe, because they are stateless, leader-elected,
# and since #58 they wait for a credential file rather than exiting when one is
# briefly missing.
#
#   ./renew-certs.sh                 # renew what expires within 30 days
#   DAYS_LEFT=90 ./renew-certs.sh    # widen the window
#   FORCE=1 ./renew-certs.sh         # renew everything regardless
#   PKI=/etc/kubernetes/pki ./renew-certs.sh
#
# What it does NOT do: rotate the CA (that is a dual-CA trust-bundle rollover,
# rustkube#20 phase 2) or the service-account signing key (every issued token
# is signed by it; rotating means dual-key validation first).

set -euo pipefail

PKI="${PKI:-/etc/kubernetes/pki}"
DAYS="${DAYS:-3650}"          # lifetime of the renewed cert
DAYS_LEFT="${DAYS_LEFT:-30}"  # renew if it expires within this many days
FORCE="${FORCE:-}"
KUBE_SVC_IP="${KUBE_SVC_IP:-10.96.0.1}"

cd "$PKI"
[ -s ca.crt ] && [ -s ca.key ] || {
    echo "no cluster CA in $PKI — this must run on a master" >&2
    exit 1
}

# Is $1 within the renewal window (or missing)?
needs_renewal() {
    local crt="$1"
    [ -n "$FORCE" ] && return 0
    [ -s "$crt" ] || return 0
    ! openssl x509 -in "$crt" -noout -checkend $((DAYS_LEFT * 86400)) >/dev/null 2>&1
}

expires() {
    openssl x509 -in "$1" -noout -enddate 2>/dev/null | cut -d= -f2
}

# Renew a client cert, preserving its subject — the subject *is* the identity
# RBAC binds to, so re-deriving it by hand is how a renewal quietly locks a
# component out.
renew_client() {
    local base="$1"
    [ -s "$base.crt" ] || { echo "  $base: absent, skipping"; return; }
    if ! needs_renewal "$base.crt"; then
        echo "  $base: valid until $(expires "$base.crt")"
        return
    fi
    local subj
    subj="$(openssl x509 -in "$base.crt" -noout -subject -nameopt RFC2253 | sed 's/^subject=//')"
    openssl genrsa -out "$base.key.new" 2048 2>/dev/null
    openssl req -new -key "$base.key.new" -subj "/$(echo "$subj" | tr ',' '/')" \
        -out "$base.csr" 2>/dev/null
    openssl x509 -req -in "$base.csr" -CA ca.crt -CAkey ca.key -CAcreateserial \
        -days "$DAYS" -extfile <(printf "extendedKeyUsage=clientAuth") \
        -out "$base.crt.new" 2>/dev/null
    rm -f "$base.csr"
    # Key and cert are swapped together: a moment with the new key and the old
    # cert is a moment where nothing works.
    mv "$base.key.new" "$base.key"
    mv "$base.crt.new" "$base.crt"
    echo "  $base: renewed, now valid until $(expires "$base.crt")  [$subj]"
    RESTART_NEEDED=1
}

# Renew the apiserver serving cert, preserving its SANs — a renewal that drops
# a SAN is a cert that no longer answers for the name a client dialled.
renew_serving() {
    local base="$1"
    [ -s "$base.crt" ] || return
    if ! needs_renewal "$base.crt"; then
        echo "  $base: valid until $(expires "$base.crt")"
        return
    fi
    local sans
    sans="$(openssl x509 -in "$base.crt" -noout -ext subjectAltName 2>/dev/null \
            | tail -n +2 | tr -d ' ' | tr '\n' ',' | sed 's/,$//')"
    if [ -z "$sans" ]; then
        sans="DNS:kubernetes,DNS:kubernetes.default,DNS:kubernetes.default.svc,DNS:kubernetes.default.svc.cluster.local,DNS:localhost,IP:127.0.0.1,IP:$KUBE_SVC_IP"
        echo "  $base: no SANs found on the old cert, using the defaults" >&2
    fi
    openssl genrsa -out "$base.key.new" 2048 2>/dev/null
    openssl req -new -key "$base.key.new" -subj "/CN=kube-apiserver" -out "$base.csr" 2>/dev/null
    openssl x509 -req -in "$base.csr" -CA ca.crt -CAkey ca.key -CAcreateserial \
        -days "$DAYS" -extfile <(printf "subjectAltName=%s\nextendedKeyUsage=serverAuth\n" "$sans") \
        -out "$base.crt.new" 2>/dev/null
    rm -f "$base.csr"
    mv "$base.key.new" "$base.key"
    mv "$base.crt.new" "$base.crt"
    echo "  $base: renewed, now valid until $(expires "$base.crt")"
    echo "         SANs: $sans"
    echo "         the apiserver reloads this within 30s — no restart"
}

RESTART_NEEDED=

echo "cluster CA: valid until $(expires ca.crt)"
if ! openssl x509 -in ca.crt -noout -checkend $((365 * 86400)) >/dev/null 2>&1; then
    echo "  WARNING: the CA itself expires within a year. Rotating it is a" >&2
    echo "  trust-bundle rollover, not a file swap — see docs/certificates.md." >&2
fi

echo "serving certificates:"
for crt in apiserver*.crt; do
    [ -e "$crt" ] || continue
    renew_serving "${crt%.crt}"
done

echo "client certificates:"
for base in admin controller-manager scheduler bootstrap; do
    renew_client "$base"
done

if [ -n "$RESTART_NEEDED" ]; then
    echo
    echo "A client certificate changed. Those components read their identity once"
    echo "at startup, so restart them to pick it up:"
    echo "    systemctl restart kube-controller-manager kube-scheduler"
    echo "  (or, under stormd, restart the processes it supervises)"
fi
