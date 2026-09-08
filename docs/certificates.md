# Certificates

## What exists

| | how it is issued | renewal |
|---|---|---|
| cluster CA | `deploy/gen-pki.sh`, 10 years | manual, and a rollover — see below |
| apiserver serving cert | `gen-pki.sh` per master, SANs per node | `deploy/renew-certs.sh`, **no restart** |
| controller-manager / scheduler / admin client certs | `gen-pki.sh`, subject is the RBAC identity | `deploy/renew-certs.sh` + restart |
| kubelet client certs | `certificates.k8s.io` CSR API; the controller manager approves and signs | the kubelet re-requests |
| service-account signing key | `gen-pki.sh` | not rotatable yet (see below) |

## The apiserver reloads its serving certificate

rustls asks a resolver for the certificate on **every handshake**, so the
serving cert is swappable while the process runs. The apiserver watches its
`--tls-cert-file` and `--tls-private-key-file` (every 30s, by content rather
than mtime) and swaps what the resolver hands out. Connections already open
keep their session; the next handshake gets the new certificate.

This is what makes rotation possible at all. Before it, the components read
their certificate once at startup, so a renewed cert on disk did nothing until
the process restarted — which meant the only way to rotate a ten-year PKI was
to redeploy the control plane, which is why nobody would.

A cert file that is unreadable or half-written is **kept, not applied**: the
running certificate is known good, and replacing it with a parse failure would
take TLS down at exactly the moment someone is touching the PKI. The failure is
logged and the old certificate keeps serving.

## Renewing

On a master:

```bash
./deploy/renew-certs.sh              # anything expiring within 30 days
DAYS_LEFT=90 ./deploy/renew-certs.sh
FORCE=1 ./deploy/renew-certs.sh
```

It preserves what the old certificate said: the **subject** on a client cert
(the subject *is* the identity RBAC binds to, so re-deriving it by hand is how a
renewal quietly locks a component out) and the **SANs** on a serving cert (a
renewal that drops a SAN is a certificate that no longer answers for the name a
client dialled). Key and certificate are swapped together, because a moment
with the new key and the old certificate is a moment where nothing works.

The apiserver picks up its new serving cert within 30 seconds. The controller
manager and the scheduler build their TLS identity once at startup and need a
restart — cheap and safe: they are stateless, leader-elected, and since #58
they wait for a credential file rather than exiting when one is briefly
missing.

## Knowing before it matters

`apiserver_certificate_expiration_seconds{name="serving"|"client-ca"}` is the
`notAfter` as a unix timestamp, and is refreshed when a cert is reloaded. The
alerting rule:

```
apiserver_certificate_expiration_seconds - time() < 30 * 86400
```

The apiserver also logs remaining validity at startup and warns within 30 days.

Generated (self-signed, `--tls`) certificates now have a real lifetime — a year
for a leaf, ten for a CA. rcgen's default `notAfter` is the year 4096, which
made the startup line read "valid for 755801 more day(s)": not a lifetime, the
absence of one, and a rotation path that is never exercised until it is needed
in anger.

## What is still missing

- **CA rotation** (#20 phase 2). Replacing the CA is a dual-CA trust-bundle
  rollover: every client must trust both the old and the new CA before anything
  is signed by the new one, and the old must stay trusted until the last leaf
  signed by it is gone. A file swap here breaks every component at once.
- **Service-account signing key rotation.** Every issued token is signed by it,
  so rotating means validating against both keys for the lifetime of the oldest
  token first.
- **Automatic renewal.** `renew-certs.sh` is run by a person or a timer; there
  is no controller watching expiry and acting. With the reload in place that is
  now a small thing to add rather than a redesign — it was the reload that was
  the blocker.
- **cert-manager CRD compatibility** (#20 phase 3) — `Issuer`/`ClusterIssuer`/
  `Certificate` for workload and Ingress certificates.
