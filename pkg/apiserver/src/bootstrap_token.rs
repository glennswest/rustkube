//! Bootstrap-token authentication, as upstream's (#264, stormcert#78).
//!
//! A machine enrolling with its forge has no credential of its own yet: it
//! authenticates its first CertificateSigningRequest with a bootstrap token,
//! the way a kubelet bootstraps. The token is `<id>.<secret>` (`[a-z0-9]{6}`
//! `.` `[a-z0-9]{16}`) and is held in the Secret
//! `kube-system/bootstrap-token-<id>`, which must be:
//!
//! - `type: bootstrap.kubernetes.io/token`, not being deleted;
//! - `token-id` = the id and `token-secret` = the secret (compared in
//!   constant time);
//! - `usage-bootstrap-authentication: "true"`;
//! - not past `expiration` (RFC 3339), when it has one.
//!
//! It authenticates as `system:bootstrap:<id>`, in `system:bootstrappers` and
//! each of `auth-extra-groups` (comma-separated, every one
//! `system:bootstrappers:…`; one that is not refuses the token, as upstream);
//! the auth middleware adds `system:authenticated`. What a bootstrapper may do
//! is `system:node-bootstrapper`'s, narrowed by [`crate::csr_admission`].
//!
//! The Secret is read from the watch cache, and from the store when the cache
//! does not have it (a Secret created a moment ago), so deleting it or
//! letting it expire revokes the token on the next request.

use crate::storage::ResourceStorage;
use base64::Engine;
use serde_json::Value;

pub const NAMESPACE: &str = "kube-system";
pub const SECRET_PREFIX: &str = "bootstrap-token-";
pub const SECRET_TYPE: &str = "bootstrap.kubernetes.io/token";
pub const USER_PREFIX: &str = "system:bootstrap:";
pub const GROUP: &str = "system:bootstrappers";

/// `<id>.<secret>`, or `None` for anything not shaped like a bootstrap token
/// (which then goes to the other authenticators).
pub fn parse(token: &str) -> Option<(&str, &str)> {
    let (id, secret) = token.split_once('.')?;
    let ok = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    (ok(id, 6) && ok(secret, 16)).then_some((id, secret))
}

/// Who the token is, if `storage` holds a Secret that makes it valid now.
pub async fn authenticate(storage: &ResourceStorage, id: &str, secret: &str) -> Option<(String, Vec<String>)> {
    let name = format!("{SECRET_PREFIX}{id}");
    let key = ResourceStorage::namespaced_key("secrets", NAMESPACE, &name);
    let prefix = ResourceStorage::all_namespaces_prefix("secrets");
    let cached = storage
        .watch_cache()
        .get(&prefix, &key)
        .await
        .ok()
        .flatten()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let now = chrono::Utc::now();
    if let Some(s) = cached {
        if let Some(user) = validate(&s, id, secret, now) {
            return Some(user);
        }
    }
    // Not in the cache, or refused by what it holds: the store decides.
    let s = storage.get(&key).await.ok()?;
    validate(&s, id, secret, now)
}

/// The identity `secret` grants for `id.token_secret` at `now`, or `None`.
pub fn validate(
    secret: &Value,
    id: &str,
    token_secret: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(String, Vec<String>)> {
    if secret["type"].as_str() != Some(SECRET_TYPE) || !secret["metadata"]["deletionTimestamp"].is_null() {
        return None;
    }
    let data = |k: &str| -> Option<String> {
        let b = secret["data"][k].as_str()?;
        let raw = base64::engine::general_purpose::STANDARD.decode(b).ok()?;
        String::from_utf8(raw).ok()
    };
    if data("token-id")? != id {
        return None;
    }
    let want = data("token-secret")?;
    if !constant_time_eq(want.as_bytes(), token_secret.as_bytes()) {
        return None;
    }
    if data("usage-bootstrap-authentication").as_deref() != Some("true") {
        return None;
    }
    if let Some(exp) = data("expiration") {
        let exp = chrono::DateTime::parse_from_rfc3339(exp.trim()).ok()?;
        if now >= exp {
            return None;
        }
    }
    let mut groups = vec![GROUP.to_string()];
    if let Some(extra) = data("auth-extra-groups") {
        for g in extra.split(',').map(str::trim).filter(|g| !g.is_empty()) {
            if !valid_extra_group(g) {
                return None;
            }
            if !groups.iter().any(|h| h == g) {
                groups.push(g.to_string());
            }
        }
    }
    Some((format!("{USER_PREFIX}{id}"), groups))
}

/// Upstream's rule: `system:bootstrappers:[a-z0-9:-]{0,255}[a-z0-9]`.
fn valid_extra_group(g: &str) -> bool {
    let Some(rest) = g.strip_prefix("system:bootstrappers:") else { return false };
    !rest.is_empty()
        && rest.len() <= 256
        && rest.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b':' || b == b'-')
        && rest.bytes().last().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    fn secret(extra: &[(&str, &str)]) -> Value {
        let mut data = serde_json::Map::new();
        for (k, v) in [
            ("token-id", "abcdef"),
            ("token-secret", "0123456789abcdef"),
            ("usage-bootstrap-authentication", "true"),
        ]
        .into_iter()
        .chain(extra.iter().copied())
        {
            data.insert(k.into(), json!(b64(v)));
        }
        json!({"type": SECRET_TYPE, "metadata": {"name": "bootstrap-token-abcdef", "namespace": "kube-system"}, "data": data})
    }

    #[test]
    fn token_shape() {
        assert_eq!(parse("abcdef.0123456789abcdef"), Some(("abcdef", "0123456789abcdef")));
        for bad in ["abcdef0123456789abcdef", "ABCDEF.0123456789abcdef", "abcde.0123456789abcdef", "abcdef.0123456789abcde",
                    "abcdef.0123456789abcdeff", "eyJhbGciOi.x.y", ""] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_valid_secret_authenticates() {
        let now = chrono::Utc::now();
        let (user, groups) = validate(&secret(&[]), "abcdef", "0123456789abcdef", now).unwrap();
        assert_eq!(user, "system:bootstrap:abcdef");
        assert_eq!(groups, vec!["system:bootstrappers"]);
        let s = secret(&[("auth-extra-groups", "system:bootstrappers:forge, system:bootstrappers:forge")]);
        let (_, groups) = validate(&s, "abcdef", "0123456789abcdef", now).unwrap();
        assert_eq!(groups, vec!["system:bootstrappers", "system:bootstrappers:forge"]);
    }

    #[test]
    fn refusals() {
        let now = chrono::Utc::now();
        let ok = |s: &Value, sec: &str| validate(s, "abcdef", sec, now).is_some();
        assert!(!ok(&secret(&[]), "0123456789abcdee"), "wrong secret");
        assert!(validate(&secret(&[]), "zzzzzz", "0123456789abcdef", now).is_none(), "wrong id");
        let mut wrong_type = secret(&[]);
        wrong_type["type"] = json!("Opaque");
        assert!(!ok(&wrong_type, "0123456789abcdef"));
        let mut not_for_auth = secret(&[]);
        not_for_auth["data"]["usage-bootstrap-authentication"] = json!(b64("false"));
        assert!(!ok(&not_for_auth, "0123456789abcdef"));
        let mut deleting = secret(&[]);
        deleting["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        assert!(!ok(&deleting, "0123456789abcdef"));
        // Expired, and an expiration that does not parse.
        let past = (now - chrono::Duration::seconds(1)).to_rfc3339();
        assert!(!ok(&secret(&[("expiration", &past)]), "0123456789abcdef"));
        let future = (now + chrono::Duration::hours(1)).to_rfc3339();
        assert!(ok(&secret(&[("expiration", &future)]), "0123456789abcdef"));
        assert!(!ok(&secret(&[("expiration", "tomorrow")]), "0123456789abcdef"));
        // An extra group outside system:bootstrappers: refused, not dropped.
        assert!(!ok(&secret(&[("auth-extra-groups", "system:masters")]), "0123456789abcdef"));
        assert!(!ok(&secret(&[("auth-extra-groups", "system:bootstrappers:")]), "0123456789abcdef"));
    }
}
