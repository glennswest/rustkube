//! The `--tls` self-signed serving certificate, kept under `--data-dir` (#88).
//!
//! Upstream's `--cert-dir` behaviour under rustkube's flag name: the pair is
//! `<data-dir>/apiserver.crt` and `<data-dir>/apiserver.key`, written on first
//! start and reused on the next, so a client can trust the file and a restart
//! does not change the certificate. The SANs carry
//! `kubernetes.default.svc.<--cluster-domain>`.
//!
//! A stored pair is reused only while it is usable: both files parse, the key
//! is the certificate's, more than [`RENEW_WITHIN_SECS`] of its lifetime is
//! left, and every SAN this start would put in is there (a changed
//! `--cluster-domain` regenerates it). Anything else is replaced. A data dir
//! that cannot be written is not fatal: the certificate is served from memory,
//! as before #88, with a warning.

use std::path::{Path, PathBuf};

use tracing::{info, warn};

/// A stored certificate this close to its `notAfter` is regenerated at start.
pub const RENEW_WITHIN_SECS: i64 = 30 * 86_400;

/// The self-signed serving pair for this start.
pub struct SelfSigned {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    /// The files it is kept in; `None` when it lives in memory only.
    pub files: Option<(PathBuf, PathBuf)>,
}

/// The DNS SANs of the self-signed certificate for `cluster_domain`.
pub fn sans(cluster_domain: &str) -> Vec<String> {
    let domain = cluster_domain.trim_matches('.');
    let mut v = vec![
        "kubernetes".to_string(),
        "kubernetes.default".to_string(),
        "kubernetes.default.svc".to_string(),
    ];
    if !domain.is_empty() {
        v.push(format!("kubernetes.default.svc.{domain}"));
    }
    v.push("localhost".to_string());
    v
}

pub fn paths(data_dir: &Path) -> (PathBuf, PathBuf) {
    (data_dir.join("apiserver.crt"), data_dir.join("apiserver.key"))
}

/// The stored pair, or a new one written to `data_dir` (in memory if it
/// cannot be written).
pub fn load_or_create(data_dir: &Path, cluster_domain: &str) -> anyhow::Result<SelfSigned> {
    let want = sans(cluster_domain);
    let (crt, key) = paths(data_dir);
    match stored(&crt, &key, &want) {
        Ok(Some((cert_pem, key_pem))) => {
            info!("serving the self-signed certificate in {}", crt.display());
            return Ok(SelfSigned { cert_pem, key_pem, files: Some((crt, key)) });
        }
        Ok(None) => {}
        Err(why) => warn!("replacing the self-signed certificate in {}: {why}", crt.display()),
    }
    let sc = apimachinery::certs::generate_server_cert("kube-apiserver", &want)?;
    let (cert_pem, key_pem) = (sc.cert_pem.into_bytes(), sc.key_pem.into_bytes());
    match write_pair(data_dir, &crt, &key, &cert_pem, &key_pem) {
        Ok(()) => {
            info!("wrote a self-signed certificate to {} (SANs {})", crt.display(), want.join(", "));
            Ok(SelfSigned { cert_pem, key_pem, files: Some((crt, key)) })
        }
        Err(e) => {
            warn!(
                "cannot write the self-signed certificate under --data-dir {}: {e}; serving it \
                 from memory, so it changes on every restart",
                data_dir.display()
            );
            Ok(SelfSigned { cert_pem, key_pem, files: None })
        }
    }
}

/// `Ok(None)`: nothing stored. `Err`: stored but not usable, and why.
fn stored(crt: &Path, key: &Path, want: &[String]) -> Result<Option<(Vec<u8>, Vec<u8>)>, String> {
    let (c, k) = match (std::fs::read(crt), std::fs::read(key)) {
        (Ok(c), Ok(k)) => (c, k),
        (Err(c), Err(k))
            if c.kind() == std::io::ErrorKind::NotFound && k.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None)
        }
        (Err(e), _) | (_, Err(e)) => return Err(format!("unreadable: {e}")),
    };
    apimachinery::tls_reload::certified_key(&c, &k).map_err(|e| format!("{e:#}"))?;
    let not_after = apimachinery::certs::cert_not_after_unix(&c).ok_or("certificate does not parse")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if not_after - now <= RENEW_WITHIN_SECS {
        return Err("expires within 30 days".into());
    }
    let have = apimachinery::certs::cert_dns_sans(&c).ok_or("certificate does not parse")?;
    if let Some(missing) = want.iter().find(|s| !have.contains(s)) {
        return Err(format!("no SAN {missing}"));
    }
    Ok(Some((c, k)))
}

/// Key first, then certificate, each written to a temporary name and renamed,
/// so a reader never sees a partial file. The key is 0600.
fn write_pair(dir: &Path, crt: &Path, key: &Path, cert_pem: &[u8], key_pem: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    write_atomic(key, key_pem, 0o600)?;
    write_atomic(crt, cert_pem, 0o644)
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        std::env::temp_dir().join(format!("rk-self-signed-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn sans_carry_the_cluster_domain() {
        assert!(sans("example.org").contains(&"kubernetes.default.svc.example.org".to_string()));
        assert!(sans("cluster.local.").contains(&"kubernetes.default.svc.cluster.local".to_string()));
        assert!(!sans("").iter().any(|s| s.starts_with("kubernetes.default.svc.")));
    }

    #[test]
    fn written_once_then_reused() {
        let d = dir();
        let a = load_or_create(&d, "cluster.local").unwrap();
        assert!(a.files.is_some());
        let (crt, key) = paths(&d);
        assert_eq!(std::fs::read(&crt).unwrap(), a.cert_pem);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
        let have = apimachinery::certs::cert_dns_sans(&a.cert_pem).unwrap();
        assert!(have.contains(&"kubernetes.default.svc.cluster.local".to_string()));

        let b = load_or_create(&d, "cluster.local").unwrap();
        assert_eq!(a.cert_pem, b.cert_pem, "a restart keeps the certificate");
        assert_eq!(a.key_pem, b.key_pem);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn changed_domain_regenerates() {
        let d = dir();
        let a = load_or_create(&d, "cluster.local").unwrap();
        let b = load_or_create(&d, "example.org").unwrap();
        assert_ne!(a.cert_pem, b.cert_pem);
        let have = apimachinery::certs::cert_dns_sans(&b.cert_pem).unwrap();
        assert!(have.contains(&"kubernetes.default.svc.example.org".to_string()));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn mismatched_or_partial_pair_regenerates() {
        let d = dir();
        let a = load_or_create(&d, "cluster.local").unwrap();
        let (crt, key) = paths(&d);
        // Another key beside the certificate: not the certificate's.
        let other = apimachinery::certs::generate_server_cert("x", &["x".into()]).unwrap();
        std::fs::write(&key, other.key_pem).unwrap();
        let b = load_or_create(&d, "cluster.local").unwrap();
        assert_ne!(a.cert_pem, b.cert_pem);
        apimachinery::tls_reload::certified_key(&b.cert_pem, &b.key_pem).unwrap();
        // Only the key left (certificate removed).
        std::fs::remove_file(&crt).unwrap();
        let c = load_or_create(&d, "cluster.local").unwrap();
        assert_eq!(std::fs::read(&crt).unwrap(), c.cert_pem);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn unwritable_dir_serves_from_memory() {
        let d = dir();
        // A file where the directory should be: create_dir_all fails.
        std::fs::write(&d, b"").unwrap();
        let a = load_or_create(&d, "cluster.local").unwrap();
        assert!(a.files.is_none());
        apimachinery::tls_reload::certified_key(&a.cert_pem, &a.key_pem).unwrap();
        std::fs::remove_file(&d).unwrap();
    }
}
