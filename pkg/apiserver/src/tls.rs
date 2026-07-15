//! TLS serving for the API server (rustls only — no OpenSSL).
//!
//! axum's `serve` has no TLS, so we accept connections with `tokio-rustls` and
//! drive each with hyper-util's auto (HTTP/1 + HTTP/2) connection builder — the
//! standard axum low-level-rustls pattern.

use axum::Router;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Build a rustls `ServerConfig` from a PEM cert chain + private key.
pub fn server_config(
    cert_pem: &[u8],
    key_pem: &[u8],
    client_ca_pem: Option<&[u8]>,
) -> anyhow::Result<ServerConfig> {
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
    let mut cfg = builder
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("building rustls config: {e}"))?;
    // Advertise HTTP/2 and HTTP/1.1 (kubectl/controllers use h2).
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(cfg)
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
