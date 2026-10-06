//! TLS serving for the API server (rustls only — no OpenSSL).
//!
//! axum's `serve` has no TLS, so we accept connections with `tokio-rustls` and
//! drive each with hyper-util's auto (HTTP/1 + HTTP/2) connection builder — the
//! standard axum low-level-rustls pattern.

use apimachinery::tls_reload::{self, ReloadingKey};
use axum::Router;
use rustls::ServerConfig;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// The serving certificate, swappable while the server runs (#20, #93): the
/// shared reloading holder, which rustls asks at every handshake.
pub type ReloadingCert = ReloadingKey;

/// The TLS config new connections are accepted with. Swapped whole when the
/// client CA changes (#105); each accepted connection takes the one current
/// when it arrives, and keeps it.
pub type CurrentConfig = Arc<RwLock<Arc<ServerConfig>>>;

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
    let resolver = ReloadingKey::from_pem(cert_pem, key_pem)
        .map_err(|e| anyhow::anyhow!("serving certificate: {e}"))?;
    let cfg = config_with(resolver.clone(), client_ca_pem)?;
    Ok((cfg, resolver))
}

/// The server config around `resolver`, verifying client certificates against
/// `client_ca_pem` when given.
///
/// A client CA bundle with no certificate in it is refused rather than read as
/// "trust nobody": at startup that is a misconfiguration, and on a reload it
/// is a file caught mid-write, which must not drop every x509 client.
pub fn config_with(
    resolver: Arc<ReloadingCert>,
    client_ca_pem: Option<&[u8]>,
) -> anyhow::Result<ServerConfig> {
    let builder = ServerConfig::builder_with_provider(tls_reload::provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow::anyhow!("TLS versions: {e}"))?;
    // Optional client-cert auth: verify presented client certs against the CA,
    // but still allow unauthenticated (anonymous / bearer-token) connections.
    let builder = if let Some(ca_pem) = client_ca_pem {
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls_pemfile::certs(&mut &ca_pem[..]) {
            roots
                .add(c.map_err(|e| anyhow::anyhow!("client CA: {e}"))?)
                .map_err(|e| anyhow::anyhow!("add client CA: {e}"))?;
        }
        if roots.is_empty() {
            anyhow::bail!("no certificates found in the client CA bundle");
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            tls_reload::provider(),
        )
        .allow_unauthenticated()
        .build()
        .map_err(|e| anyhow::anyhow!("client verifier: {e}"))?;
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };
    let mut cfg = builder.with_cert_resolver(resolver);
    // Advertise HTTP/2 and HTTP/1.1 (kubectl/controllers use h2).
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(cfg)
}

/// Watch `cert_path`/`key_path` and swap the served certificate when they
/// change: content, not mtime; a pair that does not parse or does not match
/// is kept out (#93). See `apimachinery::tls_reload`.
pub fn watch_cert_files(resolver: Arc<ReloadingCert>, cert_path: PathBuf, key_path: PathBuf) {
    resolver.watch("serving certificate", cert_path, key_path, |cert| {
        crate::server::report_cert_expiry("serving", cert)
    });
}

/// Watch `--client-ca-file` and accept new connections with the bundle it
/// holds now (#105). Before, the file was read once at startup, so a rotated
/// CA — or a bundle carrying old and new during a rollover — was ignored
/// until a restart, and clients with certificates from the new CA were
/// refused. A bundle that does not parse, or holds no certificate, is kept
/// out and the current one stays in force.
pub fn watch_client_ca(current: CurrentConfig, resolver: Arc<ReloadingCert>, ca_path: PathBuf) {
    tls_reload::watch_files("client CA bundle", vec![ca_path], move |files| {
        apply_client_ca(&current, &resolver, &files[0])
    });
}

fn apply_client_ca(current: &CurrentConfig, resolver: &Arc<ReloadingCert>, ca: &[u8]) -> anyhow::Result<()> {
    let cfg = config_with(resolver.clone(), Some(ca))?;
    *current.write().unwrap() = Arc::new(cfg);
    crate::server::report_cert_expiry("client-ca", ca);
    Ok(())
}

/// Serve `app` over TLS on `listener` until it errors.
pub async fn serve(listener: TcpListener, app: Router, cfg: CurrentConfig) -> anyhow::Result<()> {
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
        let acceptor = TlsAcceptor::from(cfg.read().unwrap().clone());
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
    use std::io::Write;

    /// A CA and a client certificate it signed: (CA PEM, client cert PEM,
    /// client key PEM).
    fn ca_and_client(cn: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.distinguished_name.push(rcgen::DnType::CommonName, format!("{cn}-ca"));
        let ca = params.self_signed(&ca_key).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(rcgen::DnType::CommonName, cn);
        let cert = params.signed_by(&key, &ca, &ca_key).unwrap();
        (ca.pem().into_bytes(), cert.pem().into_bytes(), key.serialize_pem().into_bytes())
    }

    fn serving() -> (Vec<u8>, Vec<u8>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap().self_signed(&key).unwrap();
        (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
    }

    #[test]
    fn a_mismatched_serving_pair_does_not_start() {
        let (cert, _) = serving();
        let (_, other_key) = serving();
        assert!(server_config(&cert, &other_key, None).is_err());
        let (cert, key) = serving();
        server_config(&cert, &key, None).unwrap();
        // A client CA bundle with nothing in it is a misconfiguration.
        assert!(server_config(&cert, &key, Some(b"")).is_err());
    }

    /// Does the server accept a TLS handshake from `client` (cert, key), with
    /// that certificate as the peer's?
    async fn handshake(cfg: &CurrentConfig, client: (&[u8], &[u8])) -> bool {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(cfg.read().unwrap().clone());
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            match acceptor.accept(tcp).await {
                Ok(tls) => tls.get_ref().1.peer_certificates().is_some_and(|c| !c.is_empty()),
                Err(_) => false,
            }
        });
        let id = ReloadingKey::from_pem(client.0, client.1).unwrap();
        let ccfg = tls_reload::client_config(None, true, Some(id)).unwrap();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(ccfg));
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        // Under TLS 1.3 the client finishes before the server has judged its
        // certificate, so the server's side is the answer. The client stream
        // is held open until the server is done with it.
        let _client = connector.connect(name, tcp).await;
        server.await.unwrap()
    }

    /// The client CA swap (#105): a client of the new CA is refused until the
    /// bundle names it, accepted after; a broken bundle changes nothing.
    #[tokio::test]
    async fn a_rotated_client_ca_applies_to_new_connections() {
        let (cert, key) = serving();
        let (old_ca, old_cert, old_key) = ca_and_client("old");
        let (new_ca, new_cert, new_key) = ca_and_client("new");
        let (cfg, resolver) = server_config(&cert, &key, Some(&old_ca)).unwrap();
        let current: CurrentConfig = Arc::new(RwLock::new(Arc::new(cfg)));

        assert!(handshake(&current, (&old_cert, &old_key)).await);
        assert!(!handshake(&current, (&new_cert, &new_key)).await);

        // Mid-write garbage: refused, the old bundle stays.
        assert!(apply_client_ca(&current, &resolver, b"-----BEGIN CERT").is_err());
        assert!(handshake(&current, (&old_cert, &old_key)).await);

        // A rollover bundle trusts both; then the new one alone.
        let mut both = old_ca.clone();
        both.write_all(&new_ca).unwrap();
        apply_client_ca(&current, &resolver, &both).unwrap();
        assert!(handshake(&current, (&old_cert, &old_key)).await);
        assert!(handshake(&current, (&new_cert, &new_key)).await);
        apply_client_ca(&current, &resolver, &new_ca).unwrap();
        assert!(!handshake(&current, (&old_cert, &old_key)).await);
        assert!(handshake(&current, (&new_cert, &new_key)).await);
    }
}
