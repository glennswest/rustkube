//! Authentication middleware.
//!
//! Extracts user identity from incoming requests, first match wins:
//! 1. x509 client certificate — CN is the user, each O a group
//! 2. Bearer token — `Authorization: Bearer <token>`: a static token from
//!    `--token-auth-file` ([`crate::token_file`], #188), else a JWT signed
//!    with the ServiceAccount key
//! 3. `system:anonymous`, only if `--anonymous-auth`; otherwise 401

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use jsonwebtoken::{
    decode, encode, Algorithm, DecodingKey, EncodingKey, Header, TokenData, Validation,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::storage::ResourceStorage;

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
///
/// A token from TokenRequest carries upstream's full set (#182): `iss`,
/// `aud`, `nbf`, `jti`, and the `kubernetes.io` claim naming the
/// ServiceAccount and the object the token is bound to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Claims {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    pub sub: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<Aud>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    #[serde(default)]
    pub iat: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbf: Option<u64>,
    pub exp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
    #[serde(rename = "kubernetes.io", default, skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<KubeClaims>,
}

/// `aud`: one audience or several, as RFC 7519 allows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Aud {
    One(String),
    Many(Vec<String>),
}

impl Aud {
    fn list(&self) -> Vec<String> {
        match self {
            Aud::One(a) => vec![a.clone()],
            Aud::Many(v) => v.clone(),
        }
    }
}

/// Upstream's private `kubernetes.io` claim: the ServiceAccount, and the
/// Pod, Secret or Node the token is bound to. A pod-bound token also names
/// the pod's node, for information only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KubeClaims {
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<ObjectClaim>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<ObjectClaim>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<ObjectClaim>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serviceaccount: Option<ObjectClaim>,
    /// Set on an extended token (3607 s asked, a year given): when a
    /// well-behaved client should have replaced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnafter: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ObjectClaim {
    pub name: String,
    #[serde(default)]
    pub uid: String,
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

    /// Upstream's `user.extra` for a bound token: the pod and its node, and
    /// the token's id.
    fn extra(&self) -> BTreeMap<String, Vec<String>> {
        let mut extra = BTreeMap::new();
        let mut put = |k: &str, v: &str| {
            if !v.is_empty() {
                extra.insert(format!("authentication.kubernetes.io/{k}"), vec![v.to_string()]);
            }
        };
        if let Some(kc) = &self.kubernetes {
            if let Some(pod) = &kc.pod {
                put("pod-name", &pod.name);
                put("pod-uid", &pod.uid);
            }
            if let Some(node) = &kc.node {
                put("node-name", &node.name);
                put("node-uid", &node.uid);
            }
        }
        if let Some(jti) = &self.jti {
            put("credential-id", &format!("JTI={jti}"));
        }
        extra
    }
}

/// Who a bearer token authenticates as, with what TokenReview reports
/// besides the name and groups.
#[derive(Debug, Clone)]
pub struct TokenUser {
    pub username: String,
    pub uid: String,
    pub groups: Vec<String>,
    pub extra: BTreeMap<String, Vec<String>>,
    /// The audiences asked about that the token is good for.
    pub audiences: Vec<String>,
}

/// The namespace of a `system:serviceaccount:<ns>:<name>` username, or None
/// when the username is not a well-formed ServiceAccount.
fn serviceaccount_namespace(username: &str) -> Option<&str> {
    let (ns, name) = username.strip_prefix("system:serviceaccount:")?.split_once(':')?;
    (!ns.is_empty() && !name.is_empty() && !name.contains(':')).then_some(ns)
}

/// Default `--service-account-issuer`, and so the default API audience:
/// OpenShift's, the Service name every pod reaches the apiserver by.
pub const DEFAULT_ISSUER: &str = "https://kubernetes.default.svc";

/// A deleted object's tokens stay good this long past its
/// `deletionTimestamp`, as upstream allows, so a terminating pod can finish.
const DELETED_GRACE_SECS: i64 = 60;

/// Signing keys for JWT token creation and validation, with the issuer and
/// audiences tokens are minted and checked against.
#[derive(Clone)]
pub struct SigningKeys {
    pub encoding: EncodingKey,
    pub decoding: DecodingKey,
    /// Algorithm the keys were built for — RS256 for a real ServiceAccount
    /// keypair, HS256 for the ephemeral dev key.
    algorithm: Algorithm,
    issuer: Arc<str>,
    /// `--api-audiences`: what a token must be for to authenticate here.
    audiences: Arc<[String]>,
    /// `--service-account-extend-token-expiration`.
    extend_expiration: bool,
    /// Where bound objects are looked up. Without it a bound token is
    /// refused: its binding could not be checked.
    bound: Option<Arc<ResourceStorage>>,
}

impl SigningKeys {
    fn with_keys(encoding: EncodingKey, decoding: DecodingKey, algorithm: Algorithm) -> Self {
        Self {
            encoding,
            decoding,
            algorithm,
            issuer: DEFAULT_ISSUER.into(),
            audiences: vec![DEFAULT_ISSUER.to_string()].into(),
            extend_expiration: true,
            bound: None,
        }
    }

    /// Generate an ephemeral HMAC-SHA256 signing key (dev only).
    ///
    /// Tokens signed with this die on restart and are rejected by every other
    /// apiserver replica — use `from_rsa_pem` in any real cluster (#11).
    pub fn generate() -> Self {
        let secret = uuid::Uuid::new_v4().to_string();
        Self::with_keys(
            EncodingKey::from_secret(secret.as_bytes()),
            DecodingKey::from_secret(secret.as_bytes()),
            Algorithm::HS256,
        )
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
        Ok(Self::with_keys(
            EncodingKey::from_rsa_pem(private_pem)?,
            DecodingKey::from_rsa_pem(public_pem)?,
            Algorithm::RS256,
        ))
    }

    /// Issuer and API audiences (`--service-account-issuer`,
    /// `--api-audiences`; no audiences means the issuer), and whether a
    /// pod-bound 3607 s token is extended to a year.
    pub fn with_token_config(
        mut self,
        issuer: &str,
        audiences: &[String],
        extend_expiration: bool,
    ) -> Self {
        self.issuer = issuer.into();
        self.audiences = if audiences.is_empty() {
            vec![issuer.to_string()].into()
        } else {
            audiences.to_vec().into()
        };
        self.extend_expiration = extend_expiration;
        self
    }

    /// Check bound tokens' objects in `storage`.
    pub fn with_bound_objects(mut self, storage: Arc<ResourceStorage>) -> Self {
        self.bound = Some(storage);
        self
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn api_audiences(&self) -> &[String] {
        &self.audiences
    }

    pub fn extend_expiration(&self) -> bool {
        self.extend_expiration
    }

    /// Create a 24 h JWT for a user, for this apiserver's audiences: the
    /// apiserver's own credential toward the kubelet.
    pub fn create_token(&self, username: &str, groups: &[String]) -> Option<String> {
        let now = chrono::Utc::now().timestamp() as u64;
        self.sign(&Claims {
            iss: Some(self.issuer.to_string()),
            sub: username.to_string(),
            aud: Some(Aud::Many(self.audiences.to_vec())),
            groups: groups.to_vec(),
            iat: now,
            nbf: Some(now),
            exp: now + 86400,
            jti: Some(uuid::Uuid::new_v4().to_string()),
            kubernetes: None,
        })
    }

    /// Sign `claims` as they are.
    pub fn sign(&self, claims: &Claims) -> Option<String> {
        encode(&Header::new(self.algorithm), claims, &self.encoding).ok()
    }

    /// Signature, `exp`/`nbf` and `iss` (when present, it must be ours).
    /// Audiences and bindings are not checked here.
    fn verify(&self, token: &str) -> Option<TokenData<Claims>> {
        let mut validation = Validation::new(self.algorithm);
        validation.validate_exp = true;
        validation.validate_nbf = true;
        // Checked by `audiences_for`: a token with no `aud` (stormcert's,
        // gen-node-token.sh's) is for this apiserver.
        validation.validate_aud = false;
        let data = decode::<Claims>(token, &self.decoding, &validation).ok()?;
        match &data.claims.iss {
            Some(iss) if **iss != *self.issuer => None,
            _ => Some(data),
        }
    }

    /// Which of `wanted` (the API audiences when empty) the token is for. A
    /// token with no `aud` is for the API audiences only.
    fn audiences_for(&self, claims: &Claims, wanted: &[String]) -> Vec<String> {
        let wanted: &[String] = if wanted.is_empty() { &self.audiences } else { wanted };
        let has = match &claims.aud {
            Some(aud) => aud.list(),
            None => self.audiences.to_vec(),
        };
        wanted.iter().filter(|a| has.contains(a)).cloned().collect()
    }

    /// Validate a JWT for this apiserver — signature, times, issuer and
    /// audience, but not the bound object (see [`Self::authenticate`]).
    pub fn validate_token(&self, token: &str) -> Option<TokenData<Claims>> {
        let data = self.verify(token)?;
        (!self.audiences_for(&data.claims, &[]).is_empty()).then_some(data)
    }

    /// Authenticate a JWT for `audiences` (the API audiences when empty):
    /// valid, for one of them, and — if it is bound — its ServiceAccount and
    /// bound object still the ones it was issued for.
    pub async fn authenticate(&self, token: &str, audiences: &[String]) -> Option<TokenUser> {
        let claims = self.verify(token)?.claims;
        let audiences = self.audiences_for(&claims, audiences);
        if audiences.is_empty() {
            return None;
        }
        if let Some(kc) = &claims.kubernetes {
            if !self.binding_holds(&claims.sub, kc).await {
                return None;
            }
        }
        let (username, groups) = claims.identity();
        let uid = claims
            .kubernetes
            .as_ref()
            .and_then(|kc| kc.serviceaccount.as_ref())
            .map(|sa| sa.uid.clone())
            .unwrap_or_default();
        Some(TokenUser { username, uid, groups, extra: claims.extra(), audiences })
    }

    /// The ServiceAccount the token names and the object it is bound to
    /// still exist with the uids it was issued for, as upstream's validator
    /// checks. A pod's node is information, not a binding.
    async fn binding_holds(&self, sub: &str, kc: &KubeClaims) -> bool {
        let Some(storage) = &self.bound else { return false };
        let ns = kc.namespace.as_str();
        let Some(sa) = &kc.serviceaccount else { return false };
        if sub != format!("system:serviceaccount:{ns}:{}", sa.name) {
            return false;
        }
        if !object_holds(storage, "serviceaccounts", Some(ns), sa).await {
            return false;
        }
        if let Some(pod) = &kc.pod {
            return object_holds(storage, "pods", Some(ns), pod).await;
        }
        if let Some(secret) = &kc.secret {
            return object_holds(storage, "secrets", Some(ns), secret).await;
        }
        if let Some(node) = &kc.node {
            return object_holds(storage, "nodes", None, node).await;
        }
        true
    }
}

/// `want` exists with its uid and was not deleted more than a minute ago.
/// Read from the watch cache; a refusal is confirmed from the store, since
/// the cache can trail a just-created object by milliseconds.
async fn object_holds(
    storage: &ResourceStorage,
    resource: &str,
    namespace: Option<&str>,
    want: &ObjectClaim,
) -> bool {
    let (key, prefix) = match namespace {
        Some(ns) => (
            ResourceStorage::namespaced_key(resource, ns, &want.name),
            ResourceStorage::all_namespaces_prefix(resource),
        ),
        None => (
            ResourceStorage::cluster_key(resource, &want.name),
            ResourceStorage::cluster_prefix(resource),
        ),
    };
    let cached = storage
        .watch_cache()
        .get(&prefix, &key)
        .await
        .ok()
        .flatten()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    if cached.as_ref().is_some_and(|o| object_matches(o, want)) {
        return true;
    }
    match storage.get(&key).await {
        Ok(obj) => object_matches(&obj, want),
        Err(_) => false,
    }
}

fn object_matches(obj: &serde_json::Value, want: &ObjectClaim) -> bool {
    let meta = &obj["metadata"];
    if !want.uid.is_empty() && meta["uid"].as_str() != Some(want.uid.as_str()) {
        return false;
    }
    match meta["deletionTimestamp"].as_str() {
        Some(ts) => chrono::DateTime::parse_from_rfc3339(ts).is_ok_and(|t| {
            t.timestamp() + DELETED_GRACE_SECS > chrono::Utc::now().timestamp()
        }),
        None => true,
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
        } else if let Some(token) = request
            .headers()
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(bearer_token)
        {
            // 2. Bearer token: a static token from --token-auth-file, else a
            //    JWT for this apiserver's audiences, its binding still held.
            let ext = request.extensions();
            let static_user = ext
                .get::<crate::token_file::StaticTokens>()
                .and_then(|t| t.authenticate(&token));
            let keys = ext.get::<SigningKeys>().cloned();
            let identity = match (static_user, keys) {
                (Some(id), _) => Some(id),
                (None, Some(keys)) => {
                    keys.authenticate(&token, &[]).await.map(|u| (u.username, u.groups))
                }
                (None, None) => None,
            };
            match identity {
                Some((username, groups)) => Some(UserInfo { username, groups: with_authenticated(groups) }),
                // A token was presented and nothing accepted it: 401, whatever
                // --anonymous-auth says (#115). Anonymous is for requests that
                // carry no credentials; falling back would answer a wrong,
                // expired or revoked token with anonymous's 403, which a
                // client cannot tell from a valid token without permission.
                None => return Ok(unauthorized()),
            }
        } else {
            None
        };

    // 3. No credentials: fall back to system:anonymous only if anonymous
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
                return Ok(unauthorized());
            }
        }
    };

    request.extensions_mut().insert(user_info);
    Ok(next.run(request).await)
}

/// The token of an `Authorization: Bearer <token>` header, as upstream's
/// bearer authenticator reads it: the scheme in any case, and an empty token
/// is no token (the request is anonymous, not refused).
fn bearer_token(header: &str) -> Option<String> {
    let mut parts = header.splitn(3, ' ');
    let scheme = parts.next()?;
    let token = parts.next()?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_string())
}

/// Upstream's 401: a `Status`, reason `Unauthorized`.
fn unauthorized() -> Response {
    use axum::response::IntoResponse;
    crate::error::ApiError::unauthorized("Unauthorized").into_response()
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
    fn a_token_must_be_for_this_apiserver() {
        // #182: `aud`, when present, must name an API audience (by default
        // the issuer); a token for another audience is someone else's.
        let ours = sign(serde_json::json!({
            "sub": "system:serviceaccount:kube-system:node-admin",
            "aud": ["https://kubernetes.default.svc"],
            "exp": in_ten_years(),
        }));
        assert!(test_keys().validate_token(&ours).is_some());
        let one = sign(serde_json::json!({
            "sub": "system:serviceaccount:kube-system:node-admin",
            "aud": "https://kubernetes.default.svc",
            "exp": in_ten_years(),
        }));
        assert!(test_keys().validate_token(&one).is_some());
        let theirs = sign(serde_json::json!({
            "sub": "system:serviceaccount:kube-system:node-admin",
            "aud": ["vault"],
            "exp": in_ten_years(),
        }));
        assert!(test_keys().validate_token(&theirs).is_none());
        let keys = test_keys().with_token_config("https://issuer.example", &["vault".into()], true);
        assert!(keys.validate_token(&theirs).is_some());
        assert!(keys.validate_token(&ours).is_none());
    }

    #[test]
    fn a_token_from_another_issuer_is_refused() {
        let token = sign(serde_json::json!({
            "iss": "https://elsewhere.example",
            "sub": "system:serviceaccount:kube-system:node-admin",
            "exp": in_ten_years(),
        }));
        assert!(test_keys().validate_token(&token).is_none());
        let token = sign(serde_json::json!({
            "iss": "https://kubernetes.default.svc",
            "sub": "system:serviceaccount:kube-system:node-admin",
            "exp": in_ten_years(),
        }));
        assert!(test_keys().validate_token(&token).is_some());
    }

    #[tokio::test]
    async fn a_forged_binding_for_another_serviceaccount_is_refused() {
        // `sub` and the `kubernetes.io` ServiceAccount must agree.
        let token = sign(serde_json::json!({
            "sub": "system:serviceaccount:kube-system:node-admin",
            "exp": in_ten_years(),
            "kubernetes.io": {"namespace": "n", "serviceaccount": {"name": "app", "uid": "u"}},
        }));
        let store = std::sync::Arc::new(crate::test_store::MemStore::default());
        let keys = test_keys().with_bound_objects(std::sync::Arc::new(ResourceStorage::new(store)));
        assert!(keys.authenticate(&token, &[]).await.is_none());
    }

    #[test]
    fn a_token_signed_with_another_key_is_refused() {
        let token = sign(serde_json::json!({
            "sub": "system:serviceaccount:kube-system:node-admin",
            "exp": in_ten_years(),
        }));
        assert!(SigningKeys::generate().validate_token(&token).is_none());
    }

    /// Run one request through `auth_middleware` with the given bearer token
    /// and anonymous auth off; the identity it settled on, or the status.
    async fn whoami(token: &str) -> Result<UserInfo, StatusCode> {
        whoami_as(Some(&format!("Bearer {token}")), false).await.map_err(|(s, _)| s)
    }

    /// The same with any Authorization header (or none) and anonymous auth
    /// as given; on refusal, the status and the body.
    async fn whoami_as(authorization: Option<&str>, anonymous: bool) -> Result<UserInfo, (StatusCode, String)> {
        use tower::ServiceExt;
        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::get(|axum::Extension(u): axum::Extension<UserInfo>| async move {
                    format!("{}|{}", u.username, u.groups.join(","))
                }),
            )
            .layer(axum::middleware::from_fn(|mut req: Request, next: Next| async move {
                req.extensions_mut().insert(test_keys());
                req.extensions_mut().insert(crate::token_file::StaticTokens::from_text(
                    "0123456789abcdef0123456789abcdef0123456789abcdef,system:admin,system:admin,\"system:masters\"\n",
                ));
                req.extensions_mut().insert(AnonymousAuth(anonymous));
                auth_middleware(req, next).await
            }));
        let mut req = axum::http::Request::get("/");
        if let Some(h) = authorization {
            req = req.header("authorization", h);
        }
        let resp = app.oneshot(req.body(axum::body::Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        if status != StatusCode::OK {
            return Err((status, text));
        }
        let (username, groups) = text.split_once('|').unwrap();
        Ok(UserInfo {
            username: username.into(),
            groups: groups.split(',').map(String::from).collect(),
        })
    }

    #[tokio::test]
    async fn the_install_config_token_is_system_admin_in_system_masters() {
        // #188: install-config's apiToken, as stormpump#78 writes it.
        let u = whoami("0123456789abcdef0123456789abcdef0123456789abcdef").await.unwrap();
        assert_eq!(u.username, "system:admin");
        assert_eq!(u.groups, ["system:masters", "system:authenticated"]);
    }

    #[tokio::test]
    async fn a_wrong_static_token_is_401_and_a_jwt_still_works() {
        assert_eq!(
            whoami("0123456789abcdef0123456789abcdef0123456789abcdee").await.unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
        let jwt = sign(serde_json::json!({
            "sub": "system:node:node-a",
            "groups": ["system:nodes"],
            "exp": in_ten_years(),
        }));
        let u = whoami(&jwt).await.unwrap();
        assert_eq!(u.username, "system:node:node-a");
    }

    #[tokio::test]
    async fn a_rejected_token_is_401_even_with_anonymous_auth_on() {
        // #115: a presented token nothing accepts is refused, not anonymous.
        for header in ["Bearer garbage", "bearer garbage", "Bearer 0123456789abcdef0123456789abcdef0123456789abcdee"] {
            let (status, body) = whoami_as(Some(header), true).await.unwrap_err();
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{header}");
            let status: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!((status["kind"].as_str(), status["reason"].as_str(), status["code"].as_u64()),
                       (Some("Status"), Some("Unauthorized"), Some(401)), "{header}");
        }
        // No credentials (no header, an empty token, another scheme) stay
        // anonymous when it is on…
        for header in [None, Some("Bearer "), Some("Basic dXNlcjpwYXNz")] {
            let u = whoami_as(header, true).await.unwrap();
            assert_eq!(u.username, "system:anonymous", "{header:?}");
        }
        // …and are 401 when it is off.
        assert_eq!(whoami_as(None, false).await.unwrap_err().0, StatusCode::UNAUTHORIZED);
        // A good token, either scheme spelling, still authenticates.
        let u = whoami_as(Some("bearer 0123456789abcdef0123456789abcdef0123456789abcdef"), true).await.unwrap();
        assert_eq!(u.username, "system:admin");
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
