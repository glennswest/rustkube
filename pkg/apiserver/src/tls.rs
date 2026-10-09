//! TLS serving for the API server (rustls only — no OpenSSL).
//!
//! axum's `serve` has no TLS, so we accept connections with `tokio-rustls` and
//! drive each with hyper-util's auto (HTTP/1 + HTTP/2) connection builder — the
//! standard axum low-level-rustls pattern.

use apimachinery::tls_reload::{self, ReloadingKey};
use axum::Router;
use rustls::ServerConfig;
use rustls::pki_types::CertificateRevocationListDer;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
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
    client: Option<&ClientTrust>,
) -> anyhow::Result<(ServerConfig, Arc<ReloadingCert>)> {
    // A pair that does not match is refused here too: the apiserver would
    // start and fail every handshake, which reads as a network problem.
    let resolver = ReloadingKey::from_pem(cert_pem, key_pem)
        .map_err(|e| anyhow::anyhow!("serving certificate: {e}"))?;
    let cfg = config_with(resolver.clone(), client)?;
    Ok((cfg, resolver))
}

/// What client certificates are judged by: `--client-ca-file`'s bundle and
/// the `--client-crl-file`s (#260), one entry per file, PEM or DER.
#[derive(Clone, Debug, Default)]
pub struct ClientTrust {
    pub ca: Vec<u8>,
    pub crls: Vec<Vec<u8>>,
}

/// The CRLs in one `--client-crl-file`: PEM (`X509 CRL` blocks) or a single
/// DER CRL. Whether each parses as a CRL is decided when the verifier is
/// built, which refuses one that does not.
pub fn crls_in(file: &[u8]) -> anyhow::Result<Vec<CertificateRevocationListDer<'static>>> {
    if file.is_empty() {
        anyhow::bail!("the CRL file is empty");
    }
    if file.starts_with(b"-----BEGIN") || std::str::from_utf8(file).is_ok_and(|t| t.contains("-----BEGIN")) {
        let crls = rustls_pemfile::crls(&mut &file[..])
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("CRL PEM: {e}"))?;
        if crls.is_empty() {
            anyhow::bail!("no X509 CRL block in the PEM");
        }
        return Ok(crls);
    }
    Ok(vec![CertificateRevocationListDer::from(file.to_vec())])
}

/// The server config around `resolver`, verifying client certificates against
/// `client`'s CA bundle, and its CRLs, when given.
///
/// A client CA bundle with no certificate in it is refused rather than read as
/// "trust nobody": at startup that is a misconfiguration, and on a reload it
/// is a file caught mid-write, which must not drop every x509 client. A CRL
/// that does not parse is refused the same way (#260). Revocation is checked
/// for the client's own certificate only, and a certificate whose issuer no
/// CRL covers is not refused for that — upstream's `with_crls` options
/// stormcert#61 asked for. A CRL past its nextUpdate is still used: a stale
/// list is better than none, and stormcert re-signs it every few hours.
pub fn config_with(
    resolver: Arc<ReloadingCert>,
    client: Option<&ClientTrust>,
) -> anyhow::Result<ServerConfig> {
    let builder = ServerConfig::builder_with_provider(tls_reload::provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow::anyhow!("TLS versions: {e}"))?;
    // Optional client-cert auth: verify presented client certs against the CA,
    // but still allow unauthenticated (anonymous / bearer-token) connections.
    let builder = if let Some(client) = client {
        let ca_pem = &client.ca;
        let mut crls = Vec::new();
        for file in &client.crls {
            crls.extend(crls_in(file)?);
        }
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls_pemfile::certs(&mut &ca_pem[..]) {
            roots
                .add(c.map_err(|e| anyhow::anyhow!("client CA: {e}"))?)
                .map_err(|e| anyhow::anyhow!("add client CA: {e}"))?;
        }
        if roots.is_empty() {
            anyhow::bail!("no certificates found in the client CA bundle");
        }
        let mut verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            tls_reload::provider(),
        )
        .allow_unauthenticated();
        if !crls.is_empty() {
            verifier = verifier
                .with_crls(crls)
                .only_check_end_entity_revocation()
                .allow_unknown_revocation_status();
        }
        let verifier = verifier.build().map_err(|e| anyhow::anyhow!("client verifier: {e}"))?;
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

/// The client-certificate side of the TLS config, swapped whole when the
/// client CA (#105) or a CRL (#260) changes. Each file's last good contents
/// are kept here, so a change to one is applied with the others as they are.
pub struct ClientAuth {
    current: CurrentConfig,
    resolver: Arc<ReloadingCert>,
    trust: Mutex<ClientTrust>,
}

impl ClientAuth {
    pub fn new(current: CurrentConfig, resolver: Arc<ReloadingCert>, trust: ClientTrust) -> Arc<Self> {
        Arc::new(Self { current, resolver, trust: Mutex::new(trust) })
    }

    /// Build a config with `change` made to the trust in force, and use it
    /// for new connections; a change it refuses leaves everything as it was.
    fn apply(&self, change: impl FnOnce(&mut ClientTrust)) -> anyhow::Result<()> {
        let mut trust = self.trust.lock().unwrap();
        let mut next = trust.clone();
        change(&mut next);
        let cfg = config_with(self.resolver.clone(), Some(&next))?;
        *self.current.write().unwrap() = Arc::new(cfg);
        *trust = next;
        Ok(())
    }

    fn apply_ca(&self, ca: &[u8]) -> anyhow::Result<()> {
        self.apply(|t| t.ca = ca.to_vec())?;
        crate::server::report_cert_expiry("client-ca", ca);
        Ok(())
    }

    fn apply_crl(&self, index: usize, crl: &[u8]) -> anyhow::Result<()> {
        self.apply(|t| t.crls[index] = crl.to_vec())
    }
}

/// Watch `--client-ca-file` and accept new connections with the bundle it
/// holds now (#105). Before, the file was read once at startup, so a rotated
/// CA — or a bundle carrying old and new during a rollover — was ignored
/// until a restart, and clients with certificates from the new CA were
/// refused. A bundle that does not parse, or holds no certificate, is kept
/// out and the current one stays in force.
pub fn watch_client_ca(auth: Arc<ClientAuth>, ca_path: PathBuf) {
    tls_reload::watch_files("client CA bundle", vec![ca_path], move |files| auth.apply_ca(&files[0]));
}

/// Watch the `index`th `--client-crl-file` (#260) on the same 30 s cadence:
/// a revocation stormcert writes refuses that certificate's next handshake,
/// with no restart. A missing, unreadable or unparseable file keeps the last
/// good CRL in force and is logged; revocation checking is never dropped.
pub fn watch_client_crl(auth: Arc<ClientAuth>, index: usize, path: PathBuf) {
    tls_reload::watch_files("client CRL", vec![path], move |files| auth.apply_crl(index, &files[0]));
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

    fn trust(ca: &[u8]) -> ClientTrust {
        ClientTrust { ca: ca.to_vec(), crls: Vec::new() }
    }

    #[test]
    fn a_mismatched_serving_pair_does_not_start() {
        let (cert, _) = serving();
        let (_, other_key) = serving();
        assert!(server_config(&cert, &other_key, None).is_err());
        let (cert, key) = serving();
        server_config(&cert, &key, None).unwrap();
        // A client CA bundle with nothing in it is a misconfiguration.
        assert!(server_config(&cert, &key, Some(&trust(b""))).is_err());
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
        let (cfg, resolver) = server_config(&cert, &key, Some(&trust(&old_ca))).unwrap();
        let current: CurrentConfig = Arc::new(RwLock::new(Arc::new(cfg)));
        let auth = ClientAuth::new(current.clone(), resolver, trust(&old_ca));

        assert!(handshake(&current, (&old_cert, &old_key)).await);
        assert!(!handshake(&current, (&new_cert, &new_key)).await);

        // Mid-write garbage: refused, the old bundle stays.
        assert!(auth.apply_ca(b"-----BEGIN CERT").is_err());
        assert!(handshake(&current, (&old_cert, &old_key)).await);

        // A rollover bundle trusts both; then the new one alone.
        let mut both = old_ca.clone();
        both.write_all(&new_ca).unwrap();
        auth.apply_ca(&both).unwrap();
        assert!(handshake(&current, (&old_cert, &old_key)).await);
        assert!(handshake(&current, (&new_cert, &new_key)).await);
        auth.apply_ca(&new_ca).unwrap();
        assert!(!handshake(&current, (&old_cert, &old_key)).await);
        assert!(handshake(&current, (&new_cert, &new_key)).await);
    }

    /// A CA that can sign CRLs, and client certificates with given serials.
    struct Signer {
        ca: rcgen::Certificate,
        key: rcgen::KeyPair,
    }

    impl Signer {
        fn new(cn: &str) -> Self {
            let key = rcgen::KeyPair::generate().unwrap();
            let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::CrlSign];
            params.distinguished_name.push(rcgen::DnType::CommonName, format!("{cn}-ca"));
            let ca = params.self_signed(&key).unwrap();
            Self { ca, key }
        }

        fn ca_pem(&self) -> Vec<u8> {
            self.ca.pem().into_bytes()
        }

        /// (cert PEM, key PEM) for `cn`, serial `serial`.
        fn client(&self, cn: &str, serial: u64) -> (Vec<u8>, Vec<u8>) {
            let key = rcgen::KeyPair::generate().unwrap();
            let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            params.distinguished_name.push(rcgen::DnType::CommonName, cn);
            params.serial_number = Some(rcgen::SerialNumber::from(serial));
            let cert = params.signed_by(&key, &self.ca, &self.key).unwrap();
            (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
        }

        /// A CRL revoking `serials`: (PEM, DER).
        fn crl(&self, number: u64, serials: &[u64]) -> (Vec<u8>, Vec<u8>) {
            let params = rcgen::CertificateRevocationListParams {
                this_update: rcgen::date_time_ymd(2026, 1, 1),
                next_update: rcgen::date_time_ymd(2099, 1, 1),
                crl_number: rcgen::SerialNumber::from(number),
                issuing_distribution_point: None,
                revoked_certs: serials
                    .iter()
                    .map(|s| rcgen::RevokedCertParams {
                        serial_number: rcgen::SerialNumber::from(*s),
                        revocation_time: rcgen::date_time_ymd(2026, 1, 1),
                        reason_code: Some(rcgen::RevocationReason::CessationOfOperation),
                        invalidity_date: None,
                    })
                    .collect(),
                key_identifier_method: rcgen::KeyIdMethod::Sha256,
            };
            let crl = params.signed_by(&self.ca, &self.key).unwrap();
            (crl.pem().unwrap().into_bytes(), crl.der().to_vec())
        }
    }

    #[test]
    fn crl_files_pem_or_der() {
        let s = Signer::new("crl");
        let (pem, der) = s.crl(1, &[7]);
        assert_eq!(crls_in(&pem).unwrap().len(), 1);
        assert_eq!(crls_in(&der).unwrap().len(), 1);
        assert!(crls_in(b"").is_err());
        assert!(crls_in(b"-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n").is_err());
        // Garbage that is not PEM is taken as DER and refused by the verifier.
        let (cert, key) = serving();
        let bad = ClientTrust { ca: s.ca_pem(), crls: vec![b"not a crl".to_vec()] };
        assert!(server_config(&cert, &key, Some(&bad)).is_err());
    }

    /// #260: a certificate is accepted, refused once its serial is in the
    /// CRL, with no restart; one not in the CRL still is; a bad CRL keeps the
    /// last good; a certificate of a CA no CRL covers is not refused.
    #[tokio::test]
    async fn a_revoked_client_certificate_is_refused() {
        let (cert, key) = serving();
        let s = Signer::new("nodes");
        let (gone_c, gone_k) = s.client("system:node:gone", 1001);
        let (kept_c, kept_k) = s.client("system:node:kept", 1002);
        let (empty_pem, _) = s.crl(1, &[]);
        let t = ClientTrust { ca: s.ca_pem(), crls: vec![empty_pem] };
        let (cfg, resolver) = server_config(&cert, &key, Some(&t)).unwrap();
        let current: CurrentConfig = Arc::new(RwLock::new(Arc::new(cfg)));
        let auth = ClientAuth::new(current.clone(), resolver, t);

        assert!(handshake(&current, (&gone_c, &gone_k)).await);
        assert!(handshake(&current, (&kept_c, &kept_k)).await);

        // The node leaves: its serial is in the re-signed CRL (DER this time).
        let (_, revoked_der) = s.crl(2, &[1001]);
        auth.apply_crl(0, &revoked_der).unwrap();
        assert!(!handshake(&current, (&gone_c, &gone_k)).await);
        assert!(handshake(&current, (&kept_c, &kept_k)).await);

        // A CRL caught mid-write: refused, revocation stays in force.
        assert!(auth.apply_crl(0, &revoked_der[..revoked_der.len() / 2]).is_err());
        assert!(!handshake(&current, (&gone_c, &gone_k)).await);

        // A client CA reload keeps the CRL: a second CA joins the bundle; its
        // client (no CRL covers that CA) is accepted, the revoked one is not.
        let other = Signer::new("other");
        let (oc, ok) = other.client("other", 1001);
        let mut both = s.ca_pem();
        both.extend(other.ca_pem());
        auth.apply_ca(&both).unwrap();
        assert!(handshake(&current, (&oc, &ok)).await);
        assert!(!handshake(&current, (&gone_c, &gone_k)).await);
        assert!(handshake(&current, (&kept_c, &kept_k)).await);
    }
}
