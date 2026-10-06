# Certificates

## Deployment ownership

The table below describes this repository's standalone PKI tooling. On
StormCOS, stormcert issues the files rustkube consumes. Its agent already has
`renew`, which replays recorded issuances and renews certificates/tokens at
80% of their lifetime. Enabling that loop in the golden is tracked by
[stormcos#119](https://github.com/glennswest/stormcos/issues/119); do not infer
that a running node has renewal enabled merely because the command exists.
The apiserver refuses a serving key that does not match its certificate
([#93](https://github.com/glennswest/rustkube/issues/93), below). The
controller-manager's and scheduler's client certificates and the apiserver's
client CA are followed on disk as well
([#105](https://github.com/glennswest/rustkube/issues/105)), so a stormcert
renewal needs no restart anywhere in the control plane.

As of 2026-09-29, #20 needs a scope decision: its original roadmap calls for
an in-cluster renewer and CA rotation, while stormcert owns issuance and
stormcos#119 records the owner's choice of a ten-year node CA over automatic
rotation. Canonical CA selection is separately pending in
[stormcert#49](https://github.com/glennswest/stormcert/issues/49).
No new issuer or trust-root migration should be inferred from this roadmap.

## What exists

| | how it is issued | renewal |
|---|---|---|
| cluster CA | `deploy/gen-pki.sh`, 10 years | manual, and a rollover — see below |
| apiserver serving cert | `gen-pki.sh` per master, SANs per node | `deploy/renew-certs.sh`, **no restart** |
| controller-manager / scheduler client certs | `gen-pki.sh`, subject is the RBAC identity | `deploy/renew-certs.sh`, **no restart** (#105) |
| admin client cert | `gen-pki.sh`, subject is the RBAC identity | `deploy/renew-certs.sh` (the client reads it per run) |
| kubelet bootstrap client cert (`CN=kubelet-bootstrap`, `O=system:bootstrappers`) | `gen-pki.sh` | `deploy/renew-certs.sh` |
| kubelet client certs | `certificates.k8s.io` CSR API; the controller manager auto-approves the `kubernetes.io/kube-apiserver-client-kubelet` signer, and signs only when given `--cluster-signing-cert-file`/`--cluster-signing-key-file` | the kubelet re-requests |
| service-account signing key | `gen-pki.sh` | not rotatable yet (see below) |

## Certificates reload without a restart

The serving pair, the components' client pairs and the client CA are all
followed on disk by one mechanism (`apimachinery::tls_reload`): every 30 s,
by content rather than mtime; a change that does not parse, or a pair whose
key is not its certificate's, is kept out and logged once.

### The apiserver's serving certificate

rustls asks a resolver for the certificate on **every handshake**, so the
serving cert is swappable while the process runs. The apiserver watches its
`--tls-cert-file` and `--tls-private-key-file` (every 30s, by content rather
than mtime) and swaps what the resolver hands out. Connections already open
keep their session; the next handshake gets the new certificate.

This is what makes rotation possible at all. Before it, the components read
their certificate once at startup, so a renewed cert on disk did nothing until
the process restarted — which meant the only way to rotate a ten-year PKI was
to redeploy the control plane, which is why nobody would.

A pair that cannot be read, does not parse, or whose **key is not the
certificate's** is **kept, not applied** (#93): the running certificate is
known good, and replacing it would take TLS down at exactly the moment
someone is touching the PKI. The key's public key is compared with the
certificate's (RSA, ECDSA and Ed25519 keys all report theirs; a key that
could not would be refused, not trusted). So a renewer caught between writing
the key and writing the certificate, or one that wrote only the key, leaves
the old pair serving; the refusal is logged once per change on disk, and the
next change is looked at again. The same check runs at startup, where a
mismatched pair stops the apiserver instead of failing every handshake.
A `--tls` certificate (generated in memory) is never reloaded.

### The controller-manager's and scheduler's client certificates (#105)

`--client-certificate`/`--client-key` are followed the same way. The client is
built with its own rustls config whose client-certificate resolver is the
reloading pair, so the next new connection presents the renewed certificate;
the reqwest client itself (cloned into every informer) is never rebuilt.
Connections already open keep the certificate they were opened with: reqwest
drops one idle for 90 s, and watches are reopened at least every ~5.5 min, so
the old certificate goes out of use within minutes — inside the 20 % of its
life stormcert leaves. A mismatched pair (key renewed, certificate not yet) is
refused at startup and kept out on a reload.

### The apiserver's client CA (#105)

`--client-ca-file` is followed too. A changed bundle builds a new TLS config
(same serving resolver, a verifier over the new roots), and each accepted
connection takes the config current when it arrives; open connections keep
the one they were authenticated with. A bundle that does not parse or holds
no certificate is kept out. This is what makes a client-CA rollover possible:
write the bundle with old and new CA, renew the client certificates from the
new one, then write the new CA alone. `test/e2e/client-cert-reload.sh` does
exactly that.

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
client dialled). Every step is checked (the renew functions run under `||`,
where bash ignores `set -e`): the new key and certificate are written beside
the old ones, compared by public key, and only then moved into place, key
first. A failure at any step removes the new files and leaves the old pair
untouched, as the run reports, and the run exits 1. The moment between the two
moves is harmless to the apiserver, which refuses the mismatched pair it might
see (#93).
`DAYS` (default 3650) sets the renewed lifetime, `PKI` (default
`/etc/kubernetes/pki`) where the files are, `KUBE_SVC_IP` the Service IP SAN.

The apiserver picks up its new serving cert, and the controller manager and
the scheduler their new client certs, within 30 seconds (#105) — no restart.

## Knowing before it matters

`apiserver_certificate_expiration_seconds{name="serving"|"client-ca"}` is the
`notAfter` as a unix timestamp. `serving` is refreshed when the cert is
reloaded; `client-ca` when the bundle is (#105). The
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
RS256 signature against the public key, its times, issuer and audience — and,
only for a token carrying the `kubernetes.io` claim (TokenRequest's, #182),
that its ServiceAccount and bound object still exist. A token without that
claim is not looked up. (With either missing, it falls back to an ephemeral
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
| `aud` | optional | if present, must name one of `--api-audiences` (default `https://kubernetes.default.svc`); absent means "for this apiserver" (#182) |
| `iss` | optional | if present, must be `--service-account-issuer` |
| `kubernetes.io` | optional | upstream's bound-token claim: if present, its `serviceaccount` (and `pod`/`secret`/`node`) must exist with the uids named, and `sub` must be that ServiceAccount |

Every authenticated request is also in `system:authenticated`.

### Static admin token (`--token-auth-file`, #188)

Not a certificate or JWT: install-config's `apiToken` (storminstall), which
`sc` and the console log in with. stormpump#78 writes it on first boot to
`/state/config/token-auth.csv`, mode 0600, one line in kube-apiserver's
`--token-auth-file` format:

```
<token>,system:admin,system:admin,"system:masters"
```

stormcos#256 passes that path as `--token-auth-file`. The apiserver checks a
bearer token against the file before the JWT path, in constant time, and
re-reads the file every 5 s: a token written after boot works without a
restart, a rewritten token replaces the old one, and removing the file (an
install-config without `apiToken`) revokes it. A malformed file is logged and
the previously loaded tokens are kept. Tokens are never logged.

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
- **Renewal integration.** Standalone `renew-certs.sh` is run by a person or
  timer. StormCOS has stormcert-agent's renewal loop, with deployment tracked
  by stormcos#119. The serving-pair check (#93) and client credential/trust
  reload (#105) are in place. A second issuance controller here
  would overlap stormcert; #20's scope must be reconciled first.
- **cert-manager CRD compatibility** (#20 phase 3) — `Issuer`/`ClusterIssuer`/
  `Certificate` for workload and Ingress certificates.
