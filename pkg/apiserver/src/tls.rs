//! TLS serving for the API server (rustls only — no OpenSSL).
//!
//! axum's `serve` has no TLS, so we accept connections with `tokio-rustls` and
//! drive each with hyper-util's auto (HTTP/1 + HTTP/2) connection builder — the
//! standard axum low-level-rustls pattern.

use axum::Router;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// How often the serving cert files are checked for a change.
///
/// A renewal is not an urgent event — the point is that it takes effect
/// without a restart, not that it takes effect in the same second — and a stat
/// of two files every half minute costs nothing.
const RELOAD_INTERVAL: Duration = Duration::from_secs(30);

/// Build a rustls `ServerConfig` from a PEM cert chain + private key.
/// Returns the config and the resolver holding the certificate, so a caller
/// that knows where the cert came from can arrange for it to be reloaded.
pub fn server_config(
    cert_pem: &[u8],
    key_pem: &[u8],
    client_ca_pem: Option<&[u8]>,
) -> anyhow::Result<(ServerConfig, Arc<ReloadingCert>)> {
    // A pair that does not match is refused here too: the apiserver would
    // start and fail every handshake, which reads as a network problem.
    let initial = certified_key(cert_pem, key_pem)?;

    let builder = ServerConfig::builder();
    // Optional client-cert auth: verify presented client certs against the CA,
    // but still allow unauthenticated (anonymous / bearer-token) connections.
    let builder = if let Some(ca_pem) = client_ca_pem {
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls_pemfile::certs(&mut &ca_pem[..]) {
            roots
                .add(c.map_err(|e| anyhow::anyhow!("client CA: {e}"))?)
                .map_err(|e| anyhow::anyhow!("add client CA: {e}"))?;
        }
        let verifier =
            rustls::server::WebPkiClientVerifier::builder(std::sync::Arc::new(roots))
                .allow_unauthenticated()
                .build()
                .map_err(|e| anyhow::anyhow!("client verifier: {e}"))?;
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };
    let resolver = Arc::new(ReloadingCert::new(initial));
    let mut cfg = builder.with_cert_resolver(resolver.clone());
    // Advertise HTTP/2 and HTTP/1.1 (kubectl/controllers use h2).
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok((cfg, resolver))
}

/// The serving certificate, swappable while the server is running.
///
/// **This is what makes rotation possible at all** (#20). The components read
/// their certificate once at startup, so a renewed cert on disk did nothing
/// until the process was restarted — which meant the only way to rotate a
/// ten-year PKI was a redeploy of the control plane, and that is why nobody
/// would. rustls asks a resolver for the certificate on *every* handshake, so
/// swapping what the resolver holds is enough: connections already open keep
/// their session, and the next handshake gets the new cert.
#[derive(Debug)]
pub struct ReloadingCert {
    current: RwLock<Arc<CertifiedKey>>,
}

impl ReloadingCert {
    fn new(key: Arc<CertifiedKey>) -> Self {
        Self {
            current: RwLock::new(key),
        }
    }

    fn replace(&self, key: Arc<CertifiedKey>) {
        *self.current.write().unwrap() = key;
    }
}

impl ResolvesServerCert for ReloadingCert {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.read().unwrap().clone())
    }
}

/// Parse a PEM cert chain + key into the form rustls hands out at handshake,
/// refusing a key that is not the certificate's (#93).
///
/// rustls never checks this itself: `CertifiedKey::new` takes any key with any
/// chain, and the mismatch surfaces only as every handshake failing. A renewer
/// writing the key and then the certificate is mismatched between the two
/// writes, and one that fails after writing the key stays mismatched. rustls's
/// own `from_der` lets a key type that cannot report its public key through;
/// every key the ring provider loads (RSA, ECDSA, Ed25519) reports it, so an
/// unknown answer is refused here rather than served on trust.
fn certified_key(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<Arc<CertifiedKey>> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("parsing server cert: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in TLS cert PEM");
    }
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &key_pem[..])
        .map_err(|e| anyhow::anyhow!("parsing server key: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in TLS key PEM"))?;
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| anyhow::anyhow!("unusable private key: {e}"))?;
    let certified = CertifiedKey::new(certs, signing_key);
    certified.keys_match().map_err(|e| match e {
        rustls::Error::InconsistentKeys(rustls::InconsistentKeys::KeyMismatch) => {
            anyhow::anyhow!("the private key does not match the certificate")
        }
        e => anyhow::anyhow!("cannot check the private key against the certificate: {e}"),
    })?;
    Ok(Arc::new(certified))
}

/// Watch `cert_path`/`key_path` and swap the served certificate when they
/// change.
///
/// Change is "content differs", not "mtime moved": a renewer that rewrites the
/// same bytes, or a filesystem with coarse timestamps, should not cause a
/// reload, and an atomic rename that preserves mtime should. Reading two small
/// files every 30 seconds is cheaper than being wrong about either.
///
/// A pair that is unreadable, half-written or **does not match** is kept, not
/// applied (#93). The running cert is known good; replacing it with a parse
/// failure, or with a key that is not the certificate's, would take the
/// apiserver's TLS down at exactly the moment someone is touching the PKI.
pub fn watch_cert_files(resolver: Arc<ReloadingCert>, cert_path: PathBuf, key_path: PathBuf) {
    tokio::spawn(async move {
        let mut seen = CertFiles::read(&cert_path, &key_path).unwrap_or_default();
        loop {
            tokio::time::sleep(RELOAD_INTERVAL).await;
            reload(&resolver, &cert_path, &key_path, &mut seen);
        }
    });
}

/// The bytes of the serving pair as last read.
#[derive(Default, PartialEq)]
struct CertFiles {
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl CertFiles {
    fn read(cert_path: &Path, key_path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            cert: std::fs::read(cert_path)?,
            key: std::fs::read(key_path)?,
        })
    }
}

/// What one look at the files did.
#[derive(Debug, PartialEq)]
enum Reload {
    Unchanged,
    Unreadable,
    Applied,
    Refused,
}

/// One tick of [`watch_cert_files`]. `seen` advances on every change looked at,
/// applied or refused, so a refused pair is warned about once, not every tick;
/// the next write to either file is looked at again.
fn reload(
    resolver: &ReloadingCert,
    cert_path: &Path,
    key_path: &Path,
    seen: &mut CertFiles,
) -> Reload {
    let Ok(files) = CertFiles::read(cert_path, key_path) else {
        return Reload::Unreadable; // mid-rotation, or gone; keep serving what works
    };
    if files == *seen {
        return Reload::Unchanged;
    }
    let outcome = match certified_key(&files.cert, &files.key) {
        Ok(new_key) => {
            resolver.replace(new_key);
            crate::server::report_cert_expiry("serving", &files.cert);
            tracing::info!(
                path = %cert_path.display(),
                "serving certificate reloaded without a restart",
            );
            Reload::Applied
        }
        Err(e) => {
            tracing::warn!(
                path = %cert_path.display(),
                "new serving certificate is unusable, keeping the current one: {e}",
            );
            Reload::Refused
        }
    };
    *seen = files;
    outcome
}

/// Serve `app` over TLS on `listener` until it errors.
pub async fn serve(listener: TcpListener, app: Router, cfg: ServerConfig) -> anyhow::Result<()> {
    let acceptor = TlsAcceptor::from(Arc::new(cfg));
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("accept error: {e}");
                continue;
            }
        };
        // As Go's net package does by default: a watch writes one small frame
        // per event, and with Nagle on, the second of two quick events waited
        // for the client's delayed ACK — 40 ms on Linux (#190).
        if let Err(e) = stream.set_nodelay(true) {
            tracing::debug!("TCP_NODELAY: {e}");
        }
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let tls = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(_) => return, // handshake failure — drop the connection
            };
            // Extract the client identity from its TLS cert (if it presented one)
            // and attach it so the auth middleware can authenticate x509 clients.
            let identity = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .and_then(|der| crate::auth::x509_identity_from_der(der.as_ref()));
            let app = app.layer(axum::Extension(identity));
            let io = hyper_util::rt::TokioIo::new(tls);
            let svc = hyper_util::service::TowerToHyperService::new(app);
            if let Err(e) = hyper_util::server::conn::auto::Builder::new(
                hyper_util::rt::TokioExecutor::new(),
            )
            .serve_connection_with_upgrades(io, svc)
            .await
            {
                tracing::debug!("connection error: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed serving pair: (cert PEM, key PEM, cert DER).
    fn pair(alg: &'static rcgen::SignatureAlgorithm) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let key = rcgen::KeyPair::generate_for(alg).unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (
            cert.pem().into_bytes(),
            key.serialize_pem().into_bytes(),
            cert.der().to_vec(),
        )
    }

    fn served(resolver: &ReloadingCert) -> Vec<u8> {
        resolver.current.read().unwrap().cert[0].as_ref().to_vec()
    }

    /// A directory of its own under the test's TMPDIR, removed on drop.
    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!("rk-tls-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_matching_pair_is_accepted_for_every_key_type() {
        for alg in [
            &rcgen::PKCS_ECDSA_P256_SHA256,
            &rcgen::PKCS_ECDSA_P384_SHA384,
            &rcgen::PKCS_ED25519,
        ] {
            let (cert, key, _) = pair(alg);
            certified_key(&cert, &key).unwrap_or_else(|e| panic!("{alg:?}: {e}"));
            server_config(&cert, &key, None).unwrap();
        }
    }

    #[test]
    fn a_key_that_is_not_the_certificates_is_refused() {
        let (cert, _, _) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        let (_, other_key, _) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        let e = certified_key(&cert, &other_key).unwrap_err().to_string();
        assert!(e.contains("does not match"), "{e}");
        // A different key type is a mismatch too, not a parse error.
        let (_, ed_key, _) = pair(&rcgen::PKCS_ED25519);
        let e = certified_key(&cert, &ed_key).unwrap_err().to_string();
        assert!(e.contains("does not match"), "{e}");
        // And the apiserver does not start on one.
        assert!(server_config(&cert, &other_key, None).is_err());
    }

    /// renew-certs.sh's old order — new key, then new cert — seen by a tick
    /// in between, then a tick after; and a renewal that wrote only the key.
    #[test]
    fn reload_keeps_the_running_pair_until_the_files_match() {
        let dir = Dir::new("reload");
        let (crt_path, key_path) = (dir.0.join("apiserver.crt"), dir.0.join("apiserver.key"));
        let (old_cert, old_key, old_der) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        let (new_cert, new_key, new_der) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        std::fs::write(&crt_path, &old_cert).unwrap();
        std::fs::write(&key_path, &old_key).unwrap();
        let (_, resolver) = server_config(&old_cert, &old_key, None).unwrap();
        let mut seen = CertFiles::read(&crt_path, &key_path).unwrap();

        assert_eq!(reload(&resolver, &crt_path, &key_path, &mut seen), Reload::Unchanged);

        // Key moved, cert not yet: refused, old pair still served.
        std::fs::write(&key_path, &new_key).unwrap();
        assert_eq!(reload(&resolver, &crt_path, &key_path, &mut seen), Reload::Refused);
        assert_eq!(served(&resolver), old_der);
        // Refused once, not re-warned every tick.
        assert_eq!(reload(&resolver, &crt_path, &key_path, &mut seen), Reload::Unchanged);
        assert_eq!(served(&resolver), old_der);

        // Cert follows: the matching pair is applied.
        std::fs::write(&crt_path, &new_cert).unwrap();
        assert_eq!(reload(&resolver, &crt_path, &key_path, &mut seen), Reload::Applied);
        assert_eq!(served(&resolver), new_der);

        // A failed renewal that replaced only the key: never applied.
        let (_, stray_key, _) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        std::fs::write(&key_path, &stray_key).unwrap();
        assert_eq!(reload(&resolver, &crt_path, &key_path, &mut seen), Reload::Refused);
        assert_eq!(served(&resolver), new_der);

        // Garbage, then a missing file: kept, as before.
        std::fs::write(&crt_path, b"not a certificate").unwrap();
        assert_eq!(reload(&resolver, &crt_path, &key_path, &mut seen), Reload::Refused);
        std::fs::remove_file(&crt_path).unwrap();
        assert_eq!(reload(&resolver, &crt_path, &key_path, &mut seen), Reload::Unreadable);
        assert_eq!(served(&resolver), new_der);

        // Put back a good pair: applied.
        std::fs::write(&crt_path, &old_cert).unwrap();
        std::fs::write(&key_path, &old_key).unwrap();
        assert_eq!(reload(&resolver, &crt_path, &key_path, &mut seen), Reload::Applied);
        assert_eq!(served(&resolver), old_der);
    }
}
