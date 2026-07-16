//! Authentication middleware.
//!
//! Extracts user identity from incoming requests:
//! 1. Bearer token (JWT) — `Authorization: Bearer <token>`
//! 2. Anonymous fallback — `system:anonymous`

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, TokenData, Validation};
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
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub groups: Vec<String>,
    pub iat: u64,
    pub exp: u64,
}

/// Signing keys for JWT token creation and validation.
#[derive(Clone)]
pub struct SigningKeys {
    pub encoding: EncodingKey,
    pub decoding: DecodingKey,
}

impl SigningKeys {
    /// Generate a new HMAC-SHA256 signing key.
    pub fn generate() -> Self {
        let secret = uuid::Uuid::new_v4().to_string();
        Self {
            encoding: EncodingKey::from_secret(secret.as_bytes()),
            decoding: DecodingKey::from_secret(secret.as_bytes()),
        }
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
        encode(&Header::default(), &claims, &self.encoding).ok()
    }

    /// Validate a JWT token and extract claims.
    pub fn validate_token(&self, token: &str) -> Option<TokenData<Claims>> {
        let mut validation = Validation::default();
        validation.validate_exp = true;
        decode::<Claims>(token, &self.decoding, &validation).ok()
    }
}

/// Authentication middleware — extracts UserInfo from the request.
///
/// Checks for Bearer token in Authorization header. Falls back to anonymous.
pub async fn auth_middleware(mut request: Request, next: Next) -> Result<Response, StatusCode> {
    // Resolve an *authenticated* identity, or None if no valid credentials.
    // 1. x509 client-cert identity (injected by the TLS layer) takes precedence.
    let authenticated: Option<UserInfo> =
        if let Some(Some(id)) = request.extensions().get::<Option<X509Identity>>() {
            Some(UserInfo {
                username: id.username.clone(),
                groups: id.groups.clone(),
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
                .map(|td| UserInfo {
                    username: td.claims.sub,
                    groups: td.claims.groups,
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
