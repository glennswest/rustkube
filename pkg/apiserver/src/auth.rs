//! Authentication middleware.
//!
//! Extracts user identity from incoming requests, first match wins:
//! 1. x509 client certificate — CN is the user, each O a group
//! 2. Bearer token (JWT) — `Authorization: Bearer <token>`
//! 3. `system:anonymous`, only if `--anonymous-auth`; otherwise 401

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use jsonwebtoken::{
    decode, encode, Algorithm, DecodingKey, EncodingKey, Header, TokenData, Validation,
};
use serde::{Deserialize, Serialize};

/// Authenticated user identity, stored as a request extension.
#[derive(Debug, Clone)]
pub struct UserInfo {
    pub username: String,
    pub groups: Vec<String>,
}

/// Identity extracted from a client TLS certificate: CN → username,
/// each O (organization) → a group. Matches upstream x509 authentication.
#[derive(Debug, Clone)]
pub struct X509Identity {
    pub username: String,
    pub groups: Vec<String>,
}

/// Parse a DER client certificate into an `X509Identity` (CN + organizations).
pub fn x509_identity_from_der(der: &[u8]) -> Option<X509Identity> {
    use x509_parser::prelude::*;
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    let subject = cert.subject();
    let username = subject
        .iter_common_name()
        .next()
        .and_then(|a| a.as_str().ok())?
        .to_string();
    let groups = subject
        .iter_organization()
        .filter_map(|a| a.as_str().ok().map(|s| s.to_string()))
        .collect();
    Some(X509Identity { username, groups })
}

/// JWT claims for ServiceAccount and user tokens.
///
/// A token signed outside the apiserver — `deploy/gen-node-token.sh`, or the
/// `kube-system/node-admin` token stormcert mints on each node (#79) — only
/// has to carry `sub` and `exp`. `groups` is optional, and ignored for a
/// ServiceAccount subject (see [`Claims::identity`]); `iat` is informational.
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub iat: u64,
    pub exp: u64,
}

impl Claims {
    /// The username and groups this token authenticates as.
    ///
    /// A ServiceAccount's groups follow from its name, as upstream derives
    /// them: `system:serviceaccounts` and `system:serviceaccounts:<ns>`. They
    /// are not read from the token, so a token for a ServiceAccount carries
    /// the ServiceAccount's standing and no more, whoever minted it and
    /// whatever else it claims.
    pub fn identity(&self) -> (String, Vec<String>) {
        let groups = match serviceaccount_namespace(&self.sub) {
            Some(ns) => vec![
                "system:serviceaccounts".to_string(),
                format!("system:serviceaccounts:{ns}"),
            ],
            None => self.groups.clone(),
        };
        (self.sub.clone(), groups)
    }
}

/// The namespace of a `system:serviceaccount:<ns>:<name>` username, or None
/// when the username is not a well-formed ServiceAccount.
fn serviceaccount_namespace(username: &str) -> Option<&str> {
    let (ns, name) = username.strip_prefix("system:serviceaccount:")?.split_once(':')?;
    (!ns.is_empty() && !name.is_empty() && !name.contains(':')).then_some(ns)
}

/// Signing keys for JWT token creation and validation.
#[derive(Clone)]
pub struct SigningKeys {
    pub encoding: EncodingKey,
    pub decoding: DecodingKey,
    /// Algorithm the keys were built for — RS256 for a real ServiceAccount
    /// keypair, HS256 for the ephemeral dev key.
    algorithm: Algorithm,
}

impl SigningKeys {
    /// Generate an ephemeral HMAC-SHA256 signing key (dev only).
    ///
    /// Tokens signed with this die on restart and are rejected by every other
    /// apiserver replica — use `from_rsa_pem` in any real cluster (#11).
    pub fn generate() -> Self {
        let secret = uuid::Uuid::new_v4().to_string();
        Self {
            encoding: EncodingKey::from_secret(secret.as_bytes()),
            decoding: DecodingKey::from_secret(secret.as_bytes()),
            algorithm: Algorithm::HS256,
        }
    }

    /// Load the ServiceAccount RS256 keypair: a PKCS#1/PKCS#8 private key PEM
    /// for signing and an SPKI public key PEM for verification.
    ///
    /// Because every replica loads the same on-disk keypair, a token minted by
    /// one apiserver validates on all of them, and tokens survive restarts —
    /// which is what in-cluster client-go workloads (Cilium) require (#11).
    pub fn from_rsa_pem(
        private_pem: &[u8],
        public_pem: &[u8],
    ) -> Result<Self, jsonwebtoken::errors::Error> {
        Ok(Self {
            encoding: EncodingKey::from_rsa_pem(private_pem)?,
            decoding: DecodingKey::from_rsa_pem(public_pem)?,
            algorithm: Algorithm::RS256,
        })
    }

    /// Create a JWT token for a user.
    pub fn create_token(&self, username: &str, groups: &[String]) -> Option<String> {
        let now = chrono::Utc::now().timestamp() as u64;
        let claims = Claims {
            sub: username.to_string(),
            groups: groups.to_vec(),
            iat: now,
            exp: now + 86400, // 24 hours
        };
        encode(&Header::new(self.algorithm), &claims, &self.encoding).ok()
    }

    /// Validate a JWT token and extract claims.
    pub fn validate_token(&self, token: &str) -> Option<TokenData<Claims>> {
        let mut validation = Validation::new(self.algorithm);
        validation.validate_exp = true;
        decode::<Claims>(token, &self.decoding, &validation).ok()
    }
}

/// Authentication middleware — extracts UserInfo from the request.
///
/// Every authenticated identity is in `system:authenticated`, whatever else it
/// is in.
///
/// Upstream adds this group to any successfully authenticated request, and
/// bindings in the wild are written against it — `system:basic-user`, which is
/// what lets a user ask a SelfSubjectAccessReview about themselves (#59), is
/// bound to it and nothing else. Without the group here that binding matches
/// nobody, and a manifest that grants `system:authenticated` anything would
/// silently grant it to no one.
fn with_authenticated(mut groups: Vec<String>) -> Vec<String> {
    if !groups.iter().any(|g| g == "system:authenticated") {
        groups.push("system:authenticated".into());
    }
    groups
}

/// Checks for Bearer token in Authorization header. Falls back to anonymous.
pub async fn auth_middleware(mut request: Request, next: Next) -> Result<Response, StatusCode> {
    // Resolve an *authenticated* identity, or None if no valid credentials.
    // 1. x509 client-cert identity (injected by the TLS layer) takes precedence.
    let authenticated: Option<UserInfo> =
        if let Some(Some(id)) = request.extensions().get::<Option<X509Identity>>() {
            Some(UserInfo {
                username: id.username.clone(),
                groups: with_authenticated(id.groups.clone()),
            })
        } else if let Some(auth_header) = request.headers().get("authorization") {
            // 2. Bearer token, validated against the SA/JWT signing keys.
            auth_header
                .to_str()
                .ok()
                .and_then(|h| h.strip_prefix("Bearer "))
                .and_then(|token| {
                    request
                        .extensions()
                        .get::<SigningKeys>()
                        .and_then(|keys| keys.validate_token(token))
                })
                .map(|td| {
                    let (username, groups) = td.claims.identity();
                    UserInfo { username, groups: with_authenticated(groups) }
                })
        } else {
            None
        };

    // 3. No valid credentials: fall back to system:anonymous only if anonymous
    //    auth is enabled; otherwise reject (401), matching upstream.
    let user_info = match authenticated {
        Some(u) => u,
        None => {
            let anon_allowed = request
                .extensions()
                .get::<AnonymousAuth>()
                .map(|a| a.0)
                .unwrap_or(true);
            if anon_allowed {
                anonymous_user()
            } else {
                return Err(StatusCode::UNAUTHORIZED);
            }
        }
    };

    request.extensions_mut().insert(user_info);
    Ok(next.run(request).await)
}

/// Whether unauthenticated requests fall back to `system:anonymous`. Injected by
/// the apiserver from `--anonymous-auth`; absent → allowed (dev default).
#[derive(Clone, Copy)]
pub struct AnonymousAuth(pub bool);

fn anonymous_user() -> UserInfo {
    UserInfo {
        username: "system:anonymous".into(),
        groups: vec!["system:unauthenticated".into()],
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The test ServiceAccount keypair — the shape of stormcert's
    /// `sa-token.key` / `sa-token.pub` and of `--service-account-*-file`.
    pub(crate) const TEST_SA_KEY: &str = include_str!("../testdata/sa-test.key");
    pub(crate) const TEST_SA_PUB: &str = include_str!("../testdata/sa-test.pub");

    pub(crate) fn test_keys() -> SigningKeys {
        SigningKeys::from_rsa_pem(TEST_SA_KEY.as_bytes(), TEST_SA_PUB.as_bytes()).unwrap()
    }

    /// Sign `payload` with the test key, as a minter outside the apiserver
    /// does: whatever claims it chooses, RS256, nothing added.
    pub(crate) fn sign(payload: serde_json::Value) -> String {
        let key = EncodingKey::from_rsa_pem(TEST_SA_KEY.as_bytes()).unwrap();
        encode(&Header::new(Algorithm::RS256), &payload, &key).unwrap()
    }

    fn in_ten_years() -> u64 {
        chrono::Utc::now().timestamp() as u64 + 10 * 365 * 86_400
    }

    #[test]
    fn a_node_admin_token_signed_outside_the_apiserver_authenticates() {
        // The token stormcert mints (#79): long-lived, and no `groups` —
        // a ServiceAccount's groups are not the minter's to choose.
        let token = sign(serde_json::json!({
            "sub": "system:serviceaccount:kube-system:node-admin",
            "iat": chrono::Utc::now().timestamp(),
            "exp": in_ten_years(),
        }));
        let claims = test_keys().validate_token(&token).expect("token rejected").claims;
        let (username, groups) = claims.identity();
        assert_eq!(username, "system:serviceaccount:kube-system:node-admin");
        assert_eq!(groups, ["system:serviceaccounts", "system:serviceaccounts:kube-system"]);
    }

    #[test]
    fn a_serviceaccount_token_cannot_claim_groups() {
        let token = sign(serde_json::json!({
            "sub": "system:serviceaccount:default:app",
            "groups": ["system:masters"],
            "exp": in_ten_years(),
        }));
        let (_, groups) = test_keys().validate_token(&token).unwrap().claims.identity();
        assert!(!groups.iter().any(|g| g == "system:masters"), "{groups:?}");
        assert_eq!(groups, ["system:serviceaccounts", "system:serviceaccounts:default"]);
    }

    #[test]
    fn a_user_token_keeps_its_groups() {
        // deploy/gen-node-token.sh: the node identity is a user, and its
        // group is what the bootstrap RBAC binds.
        let token = sign(serde_json::json!({
            "sub": "system:node:node-a",
            "groups": ["system:nodes"],
            "iat": 1,
            "exp": in_ten_years(),
        }));
        let (username, groups) = test_keys().validate_token(&token).unwrap().claims.identity();
        assert_eq!(username, "system:node:node-a");
        assert_eq!(groups, ["system:nodes"]);
    }

    #[test]
    fn a_token_without_an_expiry_is_refused() {
        // Long-lived, never unbounded: `exp` is the one claim besides `sub`
        // that a minter must supply.
        let token = sign(serde_json::json!({
            "sub": "system:serviceaccount:kube-system:node-admin",
        }));
        assert!(test_keys().validate_token(&token).is_none());
    }

    #[test]
    fn a_token_naming_an_audience_is_refused() {
        // No audiences are configured, so none can be checked; a minter must
        // leave `aud` out (docs/certificates.md).
        let token = sign(serde_json::json!({
            "sub": "system:serviceaccount:kube-system:node-admin",
            "aud": ["https://kubernetes.default.svc"],
            "exp": in_ten_years(),
        }));
        assert!(test_keys().validate_token(&token).is_none());
    }

    #[test]
    fn a_token_signed_with_another_key_is_refused() {
        let token = sign(serde_json::json!({
            "sub": "system:serviceaccount:kube-system:node-admin",
            "exp": in_ten_years(),
        }));
        assert!(SigningKeys::generate().validate_token(&token).is_none());
    }

    #[test]
    fn only_a_well_formed_serviceaccount_name_is_one() {
        assert_eq!(serviceaccount_namespace("system:serviceaccount:kube-system:x"), Some("kube-system"));
        assert_eq!(serviceaccount_namespace("system:serviceaccount:kube-system"), None);
        assert_eq!(serviceaccount_namespace("system:serviceaccount::x"), None);
        assert_eq!(serviceaccount_namespace("system:serviceaccount:ns:"), None);
        assert_eq!(serviceaccount_namespace("system:serviceaccount:ns:a:b"), None);
        assert_eq!(serviceaccount_namespace("system:node:node-a"), None);
    }
}
