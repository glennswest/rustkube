# Certificates

## What exists

| | how it is issued | renewal |
|---|---|---|
| cluster CA | `deploy/gen-pki.sh`, 10 years | manual, and a rollover — see below |
| apiserver serving cert | `gen-pki.sh` per master, SANs per node | `deploy/renew-certs.sh`, **no restart** |
| controller-manager / scheduler / admin client certs | `gen-pki.sh`, subject is the RBAC identity | `deploy/renew-certs.sh` + restart |
| kubelet bootstrap client cert (`CN=kubelet-bootstrap`, `O=system:bootstrappers`) | `gen-pki.sh` | `deploy/renew-certs.sh` |
| kubelet client certs | `certificates.k8s.io` CSR API; the controller manager auto-approves the `kubernetes.io/kube-apiserver-client-kubelet` signer, and signs only when given `--cluster-signing-cert-file`/`--cluster-signing-key-file` | the kubelet re-requests |
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

A cert file that cannot be read or does not parse is **kept, not applied**:
the running certificate is known good, and replacing it with a parse failure
would take TLS down at exactly the moment someone is touching the PKI. The
failure is logged and the old certificate keeps serving.

What is **not** checked is that the new key matches the new certificate. A
pair caught between the two writes, or a key written without its
certificate, is applied, and handshakes fail until the next tick puts a
matching pair in place (#93). Only the serving pair is watched: the client CA
(`--client-ca-file`) is read once at startup, and a `--tls` certificate is
never reloaded.

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
client dialled). It writes the key and then the certificate, with two
moves, so for a moment the new key sits beside the old certificate; and if
signing fails after the key has been moved, the pair on disk no longer
matches although the script reports the old certificate untouched (#93).
`DAYS` (default 3650) sets the renewed lifetime, `PKI` (default
`/etc/kubernetes/pki`) where the files are, `KUBE_SVC_IP` the Service IP SAN.

The apiserver picks up its new serving cert within 30 seconds. The controller
manager and the scheduler build their TLS identity once at startup and need a
restart — cheap and safe: they are stateless, leader-elected, and since #58
they wait for a credential file rather than exiting when one is briefly
missing.

## Knowing before it matters

`apiserver_certificate_expiration_seconds{name="serving"|"client-ca"}` is the
`notAfter` as a unix timestamp. `serving` is refreshed when the cert is
reloaded; `client-ca` is set once at startup. The
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

## Tokens signed outside the apiserver

With both `--service-account-signing-key-file` and
`--service-account-key-file` set, the apiserver verifies a bearer token by its
RS256 signature against the public key, and nothing else — no ServiceAccount
or Secret is looked up. (With either missing, it falls back to an ephemeral
HS256 key and accepts only tokens it minted itself; a verify-only replica with
just the public key is not possible.) Anything holding the matching
`--service-account-signing-key-file` can therefore mint a token offline. Two
things do:

- `deploy/gen-node-token.sh` — a kubelet's `system:node:<name>` token.
- stormcert, at a node's first boot — the `kube-system/node-admin` token for
  the node's ssh login container (#79; stormcert#5, stormcos#60). The
  apiserver bootstraps that ServiceAccount and a `node-admin`
  ClusterRoleBinding to `cluster-admin`, idempotently, like the rest of the
  bootstrap RBAC.

What such a token must carry:

| Claim | | |
|---|---|---|
| header `alg` | required | `RS256` |
| `sub` | required | the username — `system:serviceaccount:<ns>:<name>` for a ServiceAccount |
| `exp` | required | seconds since the epoch. There is no non-expiring token: long-lived means a far `exp` (the scripts here use ten years) |
| `groups` | optional | a user's groups. **Ignored for a ServiceAccount**, whose groups are always `system:serviceaccounts` and `system:serviceaccounts:<ns>`, as upstream derives them |
| `iat` | optional | informational |
| `aud` | must be absent | no audiences are configured, so a token that names one is refused |

Every authenticated request is also in `system:authenticated`.

Check a node's token with the node's own CA:

```
curl --cacert /data/stormcert/ca.crt \
  -H "Authorization: Bearer $(cat /data/stormcert/node-admin.token)" \
  https://127.0.0.1:6443/api/v1/nodes
```

Because verification is by signature alone, a token cannot be revoked by
deleting its ServiceAccount. Deleting the `node-admin` binding removes its
standing until the next apiserver boot re-creates it; revoking for good means
rotating the signing key (below).

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
