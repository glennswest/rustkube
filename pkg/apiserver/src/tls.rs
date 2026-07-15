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
pub fn server_config(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<ServerConfig> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("parsing server cert: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in TLS cert PEM");
    }
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &key_pem[..])
        .map_err(|e| anyhow::anyhow!("parsing server key: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in TLS key PEM"))?;

    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
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
