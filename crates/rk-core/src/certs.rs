/// Certificate utilities for RustKube.
///
/// Wraps rcgen for generating TLS certificates used by the API server,
/// kubelet, and inter-node communication. Stormforce-vault provides
/// the full PKI/CA infrastructure; this module offers quick helpers
/// for bootstrap scenarios.

use crate::Result;

/// A self-signed CA certificate and private key (PEM-encoded).
pub struct CertificateAuthority {
    pub cert_pem: String,
    pub key_pem: String,
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
