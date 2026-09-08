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
use std::path::PathBuf;
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
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("parsing server cert: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in TLS cert PEM");
    }
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &key_pem[..])
        .map_err(|e| anyhow::anyhow!("parsing server key: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in TLS key PEM"))?;

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
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| anyhow::anyhow!("unusable private key: {e}"))?;
    let resolver = Arc::new(ReloadingCert::new(Arc::new(CertifiedKey::new(
        certs, signing_key,
    ))));
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

/// Parse a PEM cert chain + key into the form rustls hands out at handshake.
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
    Ok(Arc::new(CertifiedKey::new(certs, signing_key)))
}

/// Watch `cert_path`/`key_path` and swap the served certificate when they
/// change.
///
/// Change is "content differs", not "mtime moved": a renewer that rewrites the
/// same bytes, or a filesystem with coarse timestamps, should not cause a
/// reload, and an atomic rename that preserves mtime should. Reading two small
/// files every 30 seconds is cheaper than being wrong about either.
///
/// A file that is unreadable or half-written is **kept, not applied**. The
/// running cert is known good; replacing it with a parse failure would take the
/// apiserver's TLS down at exactly the moment someone is touching the PKI.
pub fn watch_cert_files(resolver: Arc<ReloadingCert>, cert_path: PathBuf, key_path: PathBuf) {
    tokio::spawn(async move {
        let mut last = (
            std::fs::read(&cert_path).unwrap_or_default(),
            std::fs::read(&key_path).unwrap_or_default(),
        );
        loop {
            tokio::time::sleep(RELOAD_INTERVAL).await;
            let (cert, key) = match (std::fs::read(&cert_path), std::fs::read(&key_path)) {
                (Ok(c), Ok(k)) => (c, k),
                _ => continue, // mid-rotation, or gone; keep serving what works
            };
            if cert == last.0 && key == last.1 {
                continue;
            }
            match certified_key(&cert, &key) {
                Ok(new_key) => {
                    resolver.replace(new_key);
                    last = (cert.clone(), key);
                    crate::server::report_cert_expiry("serving", &cert);
                    tracing::info!(
                        path = %cert_path.display(),
                        "serving certificate reloaded without a restart",
                    );
                }
                Err(e) => tracing::warn!(
                    path = %cert_path.display(),
                    "new serving certificate is unusable, keeping the current one: {e}",
                ),
            }
        }
    });
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
