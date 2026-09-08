//! Certificate utilities.
//!
//! Wraps rcgen for generating TLS certificates used by the API server,
//! kubelet, and inter-node communication — quick helpers for bootstrap/dev.

use crate::Result;

/// How long a generated leaf certificate is valid.
///
/// rcgen's default is `notAfter` 4096-01-01 — the apiserver's own expiry log
/// line read "valid for 755801 more day(s)", which is not a certificate
/// lifetime, it is the absence of one. A dev cert with a two-thousand-year
/// life also means the rotation path is never exercised until it is needed in
/// anger. A year is long enough not to be a nuisance and short enough to be a
/// real date.
const LEAF_VALIDITY_DAYS: i64 = 365;

/// How long a generated CA is valid. Longer than a leaf, because rotating a CA
/// is a trust-bundle rollover rather than a file swap.
const CA_VALIDITY_DAYS: i64 = 3650;

/// Set an explicit validity window on `params`.
///
/// `notBefore` is backdated five minutes: a certificate minted on one machine
/// and used immediately on another whose clock is a little behind is otherwise
/// "not yet valid", which presents as a TLS error nobody associates with time.
fn set_validity(params: &mut rcgen::CertificateParams, days: i64) {
    let now = std::time::SystemTime::now();
    let skew = std::time::Duration::from_secs(300);
    let life = std::time::Duration::from_secs(days as u64 * 86_400);
    params.not_before = (now - skew).into();
    params.not_after = (now + life).into();
}

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
    set_validity(&mut params, LEAF_VALIDITY_DAYS);

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
    set_validity(&mut params, CA_VALIDITY_DAYS);

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
    fn a_generated_cert_has_a_real_lifetime() {
        // rcgen's default notAfter is the year 4096; the expiry metric and the
        // startup log both reported that as "755801 more day(s)", which is not
        // a lifetime.
        let leaf = generate_server_cert("test", &["localhost".to_string()]).unwrap();
        let days = (cert_not_after_unix(leaf.cert_pem.as_bytes()).unwrap()
            - chrono::Utc::now().timestamp())
            / 86_400;
        assert!((364..=366).contains(&days), "leaf should live a year, got {days} days");

        let ca = generate_ca("test-ca").unwrap();
        let ca_days = (cert_not_after_unix(ca.cert_pem.as_bytes()).unwrap()
            - chrono::Utc::now().timestamp())
            / 86_400;
        assert!((3649..=3651).contains(&ca_days), "CA should live ten years, got {ca_days}");
    }

    #[test]
    fn bad_pem_is_none() {
        assert!(cert_not_after_unix(b"not a pem").is_none());
    }
}
