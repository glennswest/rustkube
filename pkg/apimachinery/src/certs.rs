//! Certificate utilities.
//!
//! Wraps rcgen for generating TLS certificates used by the API server,
//! kubelet, and inter-node communication — quick helpers for bootstrap/dev.

use crate::Result;

/// Unix-timestamp expiry (`notAfter`) of the first certificate in a PEM bundle,
/// for cert-lifecycle monitoring (#20). Returns `None` if the PEM can't be
/// parsed.
pub fn cert_not_after_unix(pem_bytes: &[u8]) -> Option<i64> {
    use x509_parser::prelude::*;
    let (_, pem) = parse_x509_pem(pem_bytes).ok()?;
    let (_, cert) = parse_x509_certificate(&pem.contents).ok()?;
    Some(cert.validity().not_after.timestamp())
}

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

#[cfg(test)]
mod expiry_tests {
    use super::*;

    #[test]
    fn parses_not_after_of_generated_cert() {
        // A freshly generated cert expires in the future.
        let ca = generate_ca("test-ca").unwrap();
        let ts = cert_not_after_unix(ca.cert_pem.as_bytes()).expect("parse notAfter");
        assert!(ts > chrono::Utc::now().timestamp(), "expiry must be in the future");
    }

    #[test]
    fn bad_pem_is_none() {
        assert!(cert_not_after_unix(b"not a pem").is_none());
    }
}
