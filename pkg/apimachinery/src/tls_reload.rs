//! Certificates that are renewed while the process runs (#20, #93, #105).
//!
//! stormcert renews every certificate at 80 % of its life and writes the new
//! pair in place; the owner's contract (#161) is that certificates roll
//! routinely and the system keeps working for ten years. So nothing that
//! presents or trusts a certificate may read it only at startup:
//!
//! - the apiserver's serving certificate ([`ReloadingKey`] as a
//!   `ResolvesServerCert`),
//! - the client certificate kube-controller-manager and kube-scheduler present
//!   to the apiserver ([`ReloadingKey`] as a `ResolvesClientCert`, inside the
//!   rustls config [`client_config`] hands to reqwest),
//! - the apiserver's `--client-ca-file` (its own watcher, on [`watch_files`]).
//!
//! rustls asks the resolver for the certificate on every handshake, so a swap
//! takes effect on the next new connection; connections already open keep the
//! session they negotiated. reqwest closes a connection idle for 90 s, and a
//! watch is reopened at least every ~5.5 min, so in practice the old
//! certificate is out of use within minutes of the renewal — far inside the
//! 20 % of its life that stormcert leaves.
//!
//! Change is "content differs", not "mtime moved", and a pair that does not
//! parse or **does not match** (#93) is not applied: the running credential
//! is known good, and replacing it with a half-written one would take TLS
//! down at exactly the moment something is touching the PKI.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::ResolvesClientCert;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How often watched files are read for a change.
///
/// A renewal is not urgent — the point is that it takes effect without a
/// restart, not within the second — and reading two small files every half
/// minute costs nothing.
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(30);

/// The crypto provider everything here uses: ring, the only one built in.
pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
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
/// unknown answer is refused here rather than used on trust.
pub fn certified_key(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<Arc<CertifiedKey>> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("parsing certificate: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in the certificate PEM");
    }
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &key_pem[..])
        .map_err(|e| anyhow::anyhow!("parsing private key: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in the key PEM"))?;
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

/// A certificate and key, swappable while connections are being made.
///
/// The same holder serves both directions: the apiserver's serving pair
/// (`ResolvesServerCert`) and a controller's client pair
/// (`ResolvesClientCert`).
#[derive(Debug)]
pub struct ReloadingKey {
    current: RwLock<Arc<CertifiedKey>>,
}

impl ReloadingKey {
    pub fn new(key: Arc<CertifiedKey>) -> Arc<Self> {
        Arc::new(Self { current: RwLock::new(key) })
    }

    /// Build from a PEM pair, refusing a mismatched one.
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<Arc<Self>> {
        Ok(Self::new(certified_key(cert_pem, key_pem)?))
    }

    /// The pair in use now.
    pub fn current(&self) -> Arc<CertifiedKey> {
        self.current.read().unwrap().clone()
    }

    pub fn replace(&self, key: Arc<CertifiedKey>) {
        *self.current.write().unwrap() = key;
    }

    /// Follow `cert_path`/`key_path`, swapping in each renewed pair that
    /// matches. `applied` is called with the new certificate PEM.
    pub fn watch(
        self: &Arc<Self>,
        what: &'static str,
        cert_path: PathBuf,
        key_path: PathBuf,
        applied: impl Fn(&[u8]) + Send + 'static,
    ) {
        let me = self.clone();
        watch_files(what, vec![cert_path, key_path], move |files| {
            me.replace(certified_key(&files[0], &files[1])?);
            applied(&files[0]);
            Ok(())
        });
    }
}

impl ResolvesServerCert for ReloadingKey {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

impl ResolvesClientCert for ReloadingKey {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
    fn has_certs(&self) -> bool {
        true
    }
}

/// What one look at watched files did.
#[derive(Debug, PartialEq)]
pub enum Reload {
    Unchanged,
    /// A file is missing or unreadable — mid-rotation, or gone.
    Unreadable,
    Applied,
    /// Changed, and `apply` refused it; warned once.
    Refused,
}

/// A set of files and the bytes they held when last looked at.
pub struct Watched {
    what: &'static str,
    paths: Vec<PathBuf>,
    seen: Vec<Vec<u8>>,
}

impl Watched {
    /// Start from what the files hold now (what the caller loaded).
    pub fn new(what: &'static str, paths: Vec<PathBuf>) -> Self {
        let seen = read_all(&paths).unwrap_or_default();
        Self { what, paths, seen }
    }

    /// Read the files, and hand them to `apply` when any changed. `seen`
    /// advances on every change looked at, applied or refused, so a refused
    /// change is warned about once, not every tick; the next write is looked
    /// at again.
    pub fn tick(&mut self, apply: &mut impl FnMut(&[Vec<u8>]) -> anyhow::Result<()>) -> Reload {
        let Ok(files) = read_all(&self.paths) else {
            return Reload::Unreadable;
        };
        if files == self.seen {
            return Reload::Unchanged;
        }
        let path = self.paths[0].display().to_string();
        let outcome = match apply(&files) {
            Ok(()) => {
                tracing::info!(path, "{} reloaded without a restart", self.what);
                Reload::Applied
            }
            Err(e) => {
                tracing::warn!(path, "new {} is unusable, keeping the current one: {e:#}", self.what);
                Reload::Refused
            }
        };
        self.seen = files;
        outcome
    }
}

fn read_all(paths: &[PathBuf]) -> std::io::Result<Vec<Vec<u8>>> {
    paths.iter().map(std::fs::read).collect()
}

/// Read `paths` every [`RELOAD_INTERVAL`] and call `apply` with their contents
/// when any changed. An error from `apply` keeps whatever is in use.
pub fn watch_files(
    what: &'static str,
    paths: Vec<PathBuf>,
    mut apply: impl FnMut(&[Vec<u8>]) -> anyhow::Result<()> + Send + 'static,
) {
    let mut watched = Watched::new(what, paths);
    tracing::info!(path = %watched.paths[0].display(), "watching for a renewed {what}");
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(RELOAD_INTERVAL).await;
            watched.tick(&mut apply);
        }
    });
}

/// The rustls config a component's apiserver client connects with.
///
/// Trusted roots are webpki's plus `ca_pem`, as reqwest's own config had;
/// `insecure` verifies nothing (dev only). `identity` is presented when the
/// server asks for a client certificate — and because it is read at every
/// handshake, a renewed client certificate is used from the next connection
/// on (#105). Only HTTP/1.1 is offered, which is all reqwest is built for here.
pub fn client_config(
    ca_pem: Option<&[u8]>,
    insecure: bool,
    identity: Option<Arc<ReloadingKey>>,
) -> anyhow::Result<rustls::ClientConfig> {
    let provider = provider();
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow::anyhow!("TLS versions: {e}"))?;
    let builder = if insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerification(provider)))
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(pem) = ca_pem {
            let mut added = 0;
            for c in rustls_pemfile::certs(&mut &pem[..]) {
                roots
                    .add(c.map_err(|e| anyhow::anyhow!("CA bundle: {e}"))?)
                    .map_err(|e| anyhow::anyhow!("CA bundle: {e}"))?;
                added += 1;
            }
            if added == 0 {
                anyhow::bail!("no certificates found in the CA bundle");
            }
        }
        builder.with_root_certificates(roots)
    };
    let mut config = match identity {
        Some(key) => builder.with_client_cert_resolver(key),
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// `--insecure-skip-tls-verify`: any server certificate is accepted, but
/// handshake signatures are still checked against it.
#[derive(Debug)]
struct NoVerification(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A reqwest client builder for an apiserver client: TLS from
/// [`client_config`], with the client certificate followed on disk when its
/// paths are known, and a bearer token as a default header.
///
/// Shared by kube-controller-manager and kube-scheduler, whose clients used to
/// be built with `Identity::from_pem` over the startup bytes — so a renewed
/// certificate was ignored until a restart, and a process that ran past the
/// old one's expiry got 401 on every call (#105).
pub fn api_client_builder(
    ca_pem: Option<&[u8]>,
    insecure: bool,
    client_pair: Option<(&[u8], &[u8])>,
    client_files: Option<(&Path, &Path)>,
    token: Option<&str>,
) -> anyhow::Result<reqwest::ClientBuilder> {
    let identity = match client_pair {
        Some((cert, key)) => {
            let key = ReloadingKey::from_pem(cert, key)
                .map_err(|e| anyhow::anyhow!("client certificate: {e}"))?;
            if let Some((cert_path, key_path)) = client_files {
                key.watch("client certificate", cert_path.into(), key_path.into(), |_| {});
            }
            Some(key)
        }
        None => None,
    };
    let mut b = reqwest::Client::builder()
        .use_preconfigured_tls(client_config(ca_pem, insecure, identity)?);
    if let Some(token) = token {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut val = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))?;
        val.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, val);
        b = b.default_headers(headers);
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed pair: (cert PEM, key PEM, cert DER).
    pub(crate) fn pair(alg: &'static rcgen::SignatureAlgorithm) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let key = rcgen::KeyPair::generate_for(alg).unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (cert.pem().into_bytes(), key.serialize_pem().into_bytes(), cert.der().to_vec())
    }

    fn served(key: &ReloadingKey) -> Vec<u8> {
        key.current().cert[0].as_ref().to_vec()
    }

    /// A directory of its own under the test's TMPDIR, removed on drop.
    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!("rk-tlsreload-{name}-{}", std::process::id()));
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
        for alg in [&rcgen::PKCS_ECDSA_P256_SHA256, &rcgen::PKCS_ECDSA_P384_SHA384, &rcgen::PKCS_ED25519] {
            let (cert, key, _) = pair(alg);
            certified_key(&cert, &key).unwrap_or_else(|e| panic!("{alg:?}: {e}"));
        }
    }

    #[test]
    fn a_key_that_is_not_the_certificates_is_refused() {
        let (cert, _, _) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        let (_, other_key, _) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        let e = certified_key(&cert, &other_key).unwrap_err().to_string();
        assert!(e.contains("does not match"), "{e}");
        let (_, ed_key, _) = pair(&rcgen::PKCS_ED25519);
        let e = certified_key(&cert, &ed_key).unwrap_err().to_string();
        assert!(e.contains("does not match"), "{e}");
        // And a client is not built on one.
        assert!(api_client_builder(None, false, Some((&cert, &other_key)), None, None).is_err());
    }

    /// renew-certs.sh's old order — new key, then new cert — seen by a tick
    /// in between, then a tick after; and a renewal that wrote only the key.
    #[test]
    fn reload_keeps_the_running_pair_until_the_files_match() {
        let dir = Dir::new("reload");
        let (crt_path, key_path) = (dir.0.join("c.crt"), dir.0.join("c.key"));
        let (old_cert, old_key, old_der) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        let (new_cert, new_key, new_der) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        std::fs::write(&crt_path, &old_cert).unwrap();
        std::fs::write(&key_path, &old_key).unwrap();
        let key = ReloadingKey::from_pem(&old_cert, &old_key).unwrap();
        let mut watched = Watched::new("test pair", vec![crt_path.clone(), key_path.clone()]);
        let k = key.clone();
        let mut apply = move |f: &[Vec<u8>]| {
            k.replace(certified_key(&f[0], &f[1])?);
            Ok(())
        };

        assert_eq!(watched.tick(&mut apply), Reload::Unchanged);

        // Key moved, cert not yet: refused, old pair still used.
        std::fs::write(&key_path, &new_key).unwrap();
        assert_eq!(watched.tick(&mut apply), Reload::Refused);
        assert_eq!(served(&key), old_der);
        // Refused once, not re-warned every tick.
        assert_eq!(watched.tick(&mut apply), Reload::Unchanged);

        // Cert follows: the matching pair is applied.
        std::fs::write(&crt_path, &new_cert).unwrap();
        assert_eq!(watched.tick(&mut apply), Reload::Applied);
        assert_eq!(served(&key), new_der);

        // A failed renewal that replaced only the key: never applied.
        let (_, stray_key, _) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        std::fs::write(&key_path, &stray_key).unwrap();
        assert_eq!(watched.tick(&mut apply), Reload::Refused);
        assert_eq!(served(&key), new_der);

        // Garbage, then a missing file: kept.
        std::fs::write(&crt_path, b"not a certificate").unwrap();
        assert_eq!(watched.tick(&mut apply), Reload::Refused);
        std::fs::remove_file(&crt_path).unwrap();
        assert_eq!(watched.tick(&mut apply), Reload::Unreadable);
        assert_eq!(served(&key), new_der);

        // A good pair again: applied.
        std::fs::write(&crt_path, &old_cert).unwrap();
        std::fs::write(&key_path, &old_key).unwrap();
        assert_eq!(watched.tick(&mut apply), Reload::Applied);
        assert_eq!(served(&key), old_der);
    }

    #[test]
    fn client_configs_build() {
        let (cert, key, _) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        let id = ReloadingKey::from_pem(&cert, &key).unwrap();
        let c = client_config(Some(&cert), false, Some(id.clone())).unwrap();
        assert!(c.client_auth_cert_resolver.has_certs());
        assert!(!client_config(None, true, None).unwrap().client_auth_cert_resolver.has_certs());
        assert!(client_config(Some(b"no pem here"), false, None).is_err());
        api_client_builder(Some(&cert), false, Some((&cert, &key)), None, Some("t")).unwrap().build().unwrap();
    }

    /// The resolver a client was built with is the one it presents from, so
    /// a swap is what the next handshake sends.
    #[test]
    fn a_replaced_client_pair_is_what_the_resolver_hands_out() {
        let (c1, k1, d1) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        let (c2, k2, d2) = pair(&rcgen::PKCS_ECDSA_P256_SHA256);
        let id = ReloadingKey::from_pem(&c1, &k1).unwrap();
        let cfg = client_config(None, true, Some(id.clone())).unwrap();
        let got = |cfg: &rustls::ClientConfig| {
            cfg.client_auth_cert_resolver.resolve(&[], &[SignatureScheme::ECDSA_NISTP256_SHA256]).unwrap().cert[0]
                .as_ref()
                .to_vec()
        };
        assert_eq!(got(&cfg), d1);
        id.replace(certified_key(&c2, &k2).unwrap());
        assert_eq!(got(&cfg), d2);
    }
}
