//! Certificate utilities.
//!
//! Wraps rcgen for generating TLS certificates used by the API server,
//! kubelet, and inter-node communication — quick helpers for bootstrap/dev.

use crate::Result;

/// A certificate and its private key (PEM-encoded).
pub struct CertificateAuthority {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Generate a self-signed **server** certificate with the given DNS SANs
/// (e.g. `kubernetes`, `kubernetes.default.svc`, `localhost`). For dev/bootstrap
/// TLS on the API server when no cert is provided.
pub fn generate_server_cert(cn: &str, dns_sans: &[String]) -> Result<CertificateAuthority> {
    let key_pair =
        rcgen::KeyPair::generate().map_err(|e| crate::Error::Tls(e.to_string()))?;
    let mut params = rcgen::CertificateParams::new(dns_sans.to_vec())
        .map_err(|e| crate::Error::Tls(e.to_string()))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| crate::Error::Tls(e.to_string()))?;

    Ok(CertificateAuthority {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
    })
}

/// Generate a self-signed CA certificate.
pub fn generate_ca(cn: &str) -> Result<CertificateAuthority> {
    let key_pair =
        rcgen::KeyPair::generate().map_err(|e| crate::Error::Tls(e.to_string()))?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())
        .map_err(|e| crate::Error::Tls(e.to_string()))?;
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);

    let ca = params
        .self_signed(&key_pair)
        .map_err(|e| crate::Error::Tls(e.to_string()))?;

    Ok(CertificateAuthority {
        cert_pem: ca.pem(),
        key_pem: key_pair.serialize_pem(),
    })
}
