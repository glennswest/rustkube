//! Bootstrap signer: `kube-public/cluster-info`, signed per bootstrap token
//! (#388, stormcentral#1676).
//!
//! Upstream's `bootstrapsigner` plus the part of kubeadm that writes the
//! ConfigMap. A machine (or stormcentral) that holds only a bootstrap token
//! learns the cluster's CA without trusting the network: it reads
//! `cluster-info` anonymously and checks it against the token.
//!
//! - `data.kubeconfig` is a kubeconfig naming one cluster: `server` is this
//!   process's `--apiserver` URL and `certificate-authority-data` the CA
//!   bundle published as `kube-root-ca.crt` (`--root-ca-file`, else
//!   `--certificate-authority`). It is this controller's, as `kube-root-ca.crt`
//!   is the root CA publisher's: a ConfigMap that says otherwise is put back.
//!   Without a CA bundle the controller does not run.
//! - `data.jws-kubeconfig-<token-id>` is, for each bootstrap token that may
//!   sign, a detached JWS over `kubeconfig`: `<header>..<signature>`, HS256
//!   keyed with the token's secret and header `{"alg":"HS256","kid":"<id>"}`,
//!   exactly what kubeadm's discovery recomputes and compares.
//!
//! A token may sign when its Secret `kube-system/bootstrap-token-<id>` is
//! `type: bootstrap.kubernetes.io/token`, not being deleted, has a well-formed
//! `token-id` (= `<id>`) and `token-secret`, `usage-bootstrap-signing:
//! "true"`, and is not past `expiration`. A signature whose token went away,
//! stopped signing or expired is removed; the controller requeues itself at
//! the earliest expiration. Other keys of the ConfigMap are left alone.
//!
//! Reconciled on the `kube-public` Namespace, as the root CA publisher is on
//! each Namespace, with kube-system's Secrets and the ConfigMaps as
//! dependencies. Anonymous `get` of the ConfigMap is the apiserver's
//! bootstrap RBAC (`system:bootstrap-signer-clusterinfo`).

use crate::owned::{self, Controller, Dependency, Deps};
use crate::runner::ApiClient;
use apimachinery::informer::{Index, Key};
use base64::Engine;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tracing::info;

pub const NAMESPACE: &str = "kube-public";
pub const NAME: &str = "cluster-info";
pub const KUBECONFIG_KEY: &str = "kubeconfig";
pub const JWS_PREFIX: &str = "jws-kubeconfig-";
const TOKEN_NAMESPACE: &str = "kube-system";
const TOKEN_SECRET_PREFIX: &str = "bootstrap-token-";
const TOKEN_SECRET_TYPE: &str = "bootstrap.kubernetes.io/token";

pub struct BootstrapSigner {
    api: Arc<ApiClient>,
    kubeconfig: String,
}

/// The kubeconfig `cluster-info` carries, in the shape kubeadm writes it.
pub fn kubeconfig(server: &str, ca_pem: &str) -> String {
    let ca = base64::engine::general_purpose::STANDARD.encode(ca_pem.as_bytes());
    format!(
        "apiVersion: v1\n\
         clusters:\n\
         - cluster:\n    certificate-authority-data: {ca}\n    server: {server}\n  name: \"\"\n\
         contexts: null\n\
         current-context: \"\"\n\
         kind: Config\n\
         preferences: {{}}\n\
         users: null\n"
    )
}

/// HMAC-SHA256 (RFC 2104) over `msg`.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Upstream's `ComputeDetachedSignature`: the compact HS256 JWS of
/// `content` with its payload removed, `<header>..<signature>`.
pub fn detached_signature(content: &str, token_id: &str, token_secret: &str) -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    // go-jose writes the protected header's keys sorted: alg, kid. The token
    // id is [a-z0-9]{6}, so it needs no JSON escaping.
    let header = b64.encode(format!(r#"{{"alg":"HS256","kid":"{token_id}"}}"#));
    let payload = b64.encode(content);
    let mac = hmac_sha256(token_secret.as_bytes(), format!("{header}.{payload}").as_bytes());
    format!("{header}..{}", b64.encode(mac))
}

fn token_part(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// `(token-id, token-secret, expiration)` of a Secret that may sign at `now`.
pub fn signing_token(
    secret: &Value,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(String, String, Option<chrono::DateTime<chrono::Utc>>)> {
    if secret["type"].as_str() != Some(TOKEN_SECRET_TYPE) || !secret["metadata"]["deletionTimestamp"].is_null() {
        return None;
    }
    let data = |k: &str| -> Option<String> {
        let raw = base64::engine::general_purpose::STANDARD.decode(secret["data"][k].as_str()?).ok()?;
        String::from_utf8(raw).ok()
    };
    let id = data("token-id")?;
    let token_secret = data("token-secret")?;
    if !token_part(&id, 6) || !token_part(&token_secret, 16) {
        return None;
    }
    if secret["metadata"]["name"].as_str() != Some(format!("{TOKEN_SECRET_PREFIX}{id}").as_str()) {
        return None;
    }
    if data("usage-bootstrap-signing").as_deref() != Some("true") {
        return None;
    }
    let expires = match data("expiration") {
        Some(e) => {
            let e = chrono::DateTime::parse_from_rfc3339(e.trim()).ok()?.with_timezone(&chrono::Utc);
            if now >= e {
                return None;
            }
            Some(e)
        }
        None => None,
    };
    Some((id, token_secret, expires))
}

/// The ConfigMap's `data` as it should be, from what it holds and the
/// Secrets in kube-system; and the earliest expiration among the signers.
pub fn desired_data(
    current: Option<&Value>,
    kubeconfig: &str,
    secrets: &[Value],
    now: chrono::DateTime<chrono::Utc>,
) -> (Map<String, Value>, Option<chrono::DateTime<chrono::Utc>>) {
    let mut data: Map<String, Value> = current
        .and_then(|cm| cm["data"].as_object())
        .map(|d| d.iter().filter(|(k, _)| !k.starts_with(JWS_PREFIX)).map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    data.insert(KUBECONFIG_KEY.into(), json!(kubeconfig));
    let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
    for s in secrets {
        if s["metadata"]["namespace"].as_str() != Some(TOKEN_NAMESPACE) {
            continue;
        }
        let Some((id, secret, expires)) = signing_token(s, now) else { continue };
        data.insert(format!("{JWS_PREFIX}{id}"), json!(detached_signature(kubeconfig, &id, &secret)));
        if let Some(e) = expires {
            earliest = Some(earliest.map_or(e, |x| x.min(e)));
        }
    }
    (data, earliest)
}

impl BootstrapSigner {
    pub fn new(api: Arc<ApiClient>, ca_pem: &str) -> Self {
        let kubeconfig = kubeconfig(&api.base_url, ca_pem);
        Self { api, kubeconfig }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }
}

#[async_trait::async_trait]
impl Controller for BootstrapSigner {
    fn name(&self) -> &'static str {
        "bootstrapsigner"
    }
    fn primary(&self) -> &'static str {
        "/api/v1/namespaces"
    }
    fn workers(&self) -> usize {
        1
    }
    fn dependencies(&self) -> Vec<Dependency> {
        // Any change to either routes to the kube-public Namespace.
        let to_kube_public = |primary: &apimachinery::informers::Feed| -> Vec<Key> {
            owned::keys_at(primary, Index::Name("".into(), NAMESPACE.into()))
        };
        vec![
            Dependency {
                path: format!("/api/v1/namespaces/{TOKEN_NAMESPACE}/secrets"),
                route: Arc::new(move |delta, primary| {
                    let token = |s: &Value| s["type"].as_str() == Some(TOKEN_SECRET_TYPE);
                    if delta.old.iter().chain(delta.new.iter()).any(token) {
                        to_kube_public(primary)
                    } else {
                        Vec::new()
                    }
                }),
            },
            Dependency {
                path: "/api/v1/configmaps".into(),
                route: Arc::new(move |delta, primary| {
                    let ours = |c: &Value| c["metadata"]["namespace"] == NAMESPACE && c["metadata"]["name"] == NAME;
                    if delta.old.iter().chain(delta.new.iter()).any(ours) {
                        to_kube_public(primary)
                    } else {
                        Vec::new()
                    }
                }),
            },
        ]
    }
    async fn reconcile(&self, ns: &Value, _children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        if ns["metadata"]["name"] != NAMESPACE
            || !ns["metadata"]["deletionTimestamp"].is_null()
            || ns["status"]["phase"] == "Terminating"
        {
            return Ok(());
        }
        let secrets = deps.feed(0).select(&Index::Namespace(TOKEN_NAMESPACE.into()))?;
        let current = deps.feed(1).select(&Index::Name(NAMESPACE.into(), NAME.into()))?.into_iter().next();
        let now = chrono::Utc::now();
        let (data, earliest) = desired_data(current.as_ref(), &self.kubeconfig, &secrets, now);
        if let Some(at) = earliest {
            apimachinery::reactor::requeue_at_time(at);
        }
        let signers = data.keys().filter(|k| k.starts_with(JWS_PREFIX)).count();
        match current {
            None => {
                let cm = json!({
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "metadata": {"name": NAME, "namespace": NAMESPACE},
                    "data": data,
                });
                self.api.create(&format!("/api/v1/namespaces/{NAMESPACE}/configmaps"), &cm).await?;
                info!("Published {NAMESPACE}/{NAME} ({signers} token signatures)");
            }
            Some(cm) if cm["data"].as_object() != Some(&data) => {
                let mut desired = cm.clone();
                desired["data"] = Value::Object(data);
                // Conditional on the version read: a concurrent writer is
                // not overwritten; the conflict retries from the new state.
                self.api.update(&format!("/api/v1/namespaces/{NAMESPACE}/configmaps/{NAME}"), &desired).await?;
                info!("Updated {NAMESPACE}/{NAME} ({signers} token signatures)");
            }
            Some(_) => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    fn token(id: &str, secret: &str, extra: &[(&str, &str)]) -> Value {
        let mut data = Map::new();
        for (k, v) in [("token-id", id), ("token-secret", secret), ("usage-bootstrap-signing", "true")]
            .into_iter()
            .chain(extra.iter().copied())
        {
            data.insert(k.into(), json!(b64(v)));
        }
        json!({"type": TOKEN_SECRET_TYPE, "data": data,
               "metadata": {"name": format!("bootstrap-token-{id}"), "namespace": "kube-system"}})
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 2, and test case 6 (a key longer than the block).
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(hex(&mac), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        let mac = hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First");
        assert_eq!(hex(&mac), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn the_signature_is_upstreams() {
        // What k8s.io/cluster-bootstrap/token/jws ComputeDetachedSignature
        // produces: go-jose's protected header with its keys sorted,
        // {"alg":"HS256","kid":"abcdef"}, HMAC-SHA256 over header.payload,
        // the payload cut out. The expected string was computed
        // independently (Python's hmac and base64.urlsafe_b64encode).
        let sig = detached_signature("hello", "abcdef", "abcdef0123456789");
        assert_eq!(sig, "eyJhbGciOiJIUzI1NiIsImtpZCI6ImFiY2RlZiJ9..3oCG3ZvTsm1oLTGeG2SBIpCzVtC_Gx2sdQjqT1BKiek");
        // A different token, or different content, signs differently.
        assert_ne!(sig, detached_signature("hello", "abcdef", "abcdef0123456788"));
        assert_ne!(sig, detached_signature("hello!", "abcdef", "abcdef0123456789"));
    }

    #[test]
    fn the_kubeconfig_names_the_server_and_ca() {
        let k = kubeconfig("https://10.0.0.5:6443", "-----BEGIN CERTIFICATE-----\nX\n");
        assert!(k.contains("    server: https://10.0.0.5:6443\n"));
        assert!(k.contains(&format!("    certificate-authority-data: {}\n", b64("-----BEGIN CERTIFICATE-----\nX\n"))));
        assert!(k.starts_with("apiVersion: v1\nclusters:\n- cluster:\n"));
        assert!(k.contains("kind: Config\n"));
    }

    #[test]
    fn which_tokens_sign() {
        let now = chrono::Utc::now();
        let ok = token("abcdef", "0123456789abcdef", &[]);
        assert_eq!(signing_token(&ok, now).map(|t| t.0), Some("abcdef".into()));
        let future = (now + chrono::Duration::hours(1)).to_rfc3339();
        assert!(signing_token(&token("abcdef", "0123456789abcdef", &[("expiration", &future)]), now).unwrap().2.is_some());
        let past = (now - chrono::Duration::seconds(1)).to_rfc3339();
        let mut refused = vec![
            ("expired", token("abcdef", "0123456789abcdef", &[("expiration", &past)])),
            ("bad expiration", token("abcdef", "0123456789abcdef", &[("expiration", "soon")])),
            ("short secret", token("abcdef", "0123456789abcde", &[])),
            ("upper-case id", token("ABCDEF", "0123456789abcdef", &[])),
        ];
        let mut no_signing = token("abcdef", "0123456789abcdef", &[]);
        no_signing["data"]["usage-bootstrap-signing"] = json!(b64("false"));
        refused.push(("not for signing", no_signing));
        let mut wrong_name = ok.clone();
        wrong_name["metadata"]["name"] = json!("bootstrap-token-zzzzzz");
        refused.push(("name and id differ", wrong_name));
        let mut opaque = ok.clone();
        opaque["type"] = json!("Opaque");
        refused.push(("not a token", opaque));
        let mut deleting = ok.clone();
        deleting["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        refused.push(("deleting", deleting));
        for (what, s) in refused {
            assert!(signing_token(&s, now).is_none(), "{what}");
        }
    }

    #[test]
    fn desired_data_signs_each_token_and_drops_stale_signatures() {
        let now = chrono::Utc::now();
        let soon = now + chrono::Duration::minutes(5);
        let later = (now + chrono::Duration::hours(5)).to_rfc3339();
        let current = json!({"data": {"kubeconfig": "old", "jws-kubeconfig-gone00": "x", "other": "kept"}});
        let mut elsewhere = token("zzzzzz", "0123456789abcdef", &[]);
        elsewhere["metadata"]["namespace"] = json!("default");
        let secrets = vec![
            token("abcdef", "0123456789abcdef", &[("expiration", &soon.to_rfc3339())]),
            token("ghijkl", "fedcba9876543210", &[("expiration", &later)]),
            elsewhere,
        ];
        let (data, earliest) = desired_data(Some(&current), "KC", &secrets, now);
        assert_eq!(data["kubeconfig"], "KC");
        assert_eq!(data["other"], "kept");
        assert!(data.get("jws-kubeconfig-gone00").is_none());
        assert!(data.get("jws-kubeconfig-zzzzzz").is_none(), "only kube-system's tokens");
        assert_eq!(data["jws-kubeconfig-abcdef"], detached_signature("KC", "abcdef", "0123456789abcdef"));
        assert_eq!(data["jws-kubeconfig-ghijkl"], detached_signature("KC", "ghijkl", "fedcba9876543210"));
        assert_eq!(earliest.map(|e| e.timestamp()), Some(soon.timestamp()));
        // Nothing yet: kubeconfig alone.
        let (data, earliest) = desired_data(None, "KC", &[], now);
        assert_eq!(Value::Object(data), json!({"kubeconfig": "KC"}));
        assert!(earliest.is_none());
    }
}
