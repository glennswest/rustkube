//! ServiceAccount TokenRequest, and TokenReview.
//!
//! `POST /api/v1/namespaces/{ns}/serviceaccounts/{name}/token` issues a signed
//! token whose subject is `system:serviceaccount:{ns}:{name}`, shaped as
//! upstream's and OpenShift's (#182):
//!
//! - `spec.audiences` become `aud` (none asked: the apiserver's
//!   `--api-audiences`); the apiserver itself accepts only a token for one of
//!   its own audiences.
//! - `spec.expirationSeconds` is honoured: at least 600 s, at most 2^32. Left
//!   out, the token lasts 24 h — upstream defaults to an hour, but today's
//!   rustkube-node asks without one and never refreshes (rustkube-node#122).
//! - `spec.boundObjectRef` — a Pod, Secret or Node — binds the token to that
//!   object: it must exist (with the given uid, if one is given), a Pod must
//!   run as this ServiceAccount, and the token stops authenticating once the
//!   object is gone. A pod-bound token also names the pod's node.
//! - A pod-bound request for 3607 s (what a projected token volume asks)
//!   for the apiserver's audiences is given a year with `warnafter` at
//!   3607 s, unless `--service-account-extend-token-expiration=false`; the
//!   response still says 3607 s, so a client refreshes on time.
//!
//! The claims are upstream's: `iss`, `sub`, `aud`, `iat`, `nbf`, `exp`, `jti`
//! and `kubernetes.io: {namespace, serviceaccount, pod|secret|node, node}`.

use crate::auth::{Aud, Claims, KubeClaims, ObjectClaim, SigningKeys};
use crate::error::ApiError;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde_json::{json, Value};

/// Lifetime of a token whose request names none.
const DEFAULT_TTL_SECS: i64 = 86_400;
/// Shortest lifetime a request may ask for, as upstream.
const MIN_TTL_SECS: i64 = 600;
/// Longest, as upstream's validation.
const MAX_TTL_SECS: i64 = 1 << 32;
/// What a projected `serviceAccountToken` source asks for, and upstream's
/// signal to extend it (`WarnOnlyBoundTokenExpirationSeconds`).
const WARN_ONLY_TTL_SECS: i64 = 3607;
/// What such a token is extended to (`ExpirationExtensionSeconds`).
const EXTENDED_TTL_SECS: i64 = 365 * 86_400;

pub async fn create_serviceaccount_token(
    State(state): State<AppState>,
    Extension(keys): Extension<SigningKeys>,
    Path((namespace, name)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let req: Value = if body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(&format!("TokenRequest: {e}")))?
    };
    let spec = &req["spec"];

    // The ServiceAccount must exist (404 otherwise).
    let sa_key = ResourceStorage::namespaced_key("serviceaccounts", &namespace, &name);
    let sa = state.storage.get(&sa_key).await?;
    let sa_uid = sa["metadata"]["uid"].as_str().unwrap_or("").to_string();

    let audiences: Vec<String> = match spec["audiences"].as_array() {
        Some(a) if !a.is_empty() => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        _ => keys.api_audiences().to_vec(),
    };
    let ttl = match &spec["expirationSeconds"] {
        Value::Null => DEFAULT_TTL_SECS,
        v => {
            let secs = v.as_i64().ok_or_else(|| {
                ApiError::invalid(&format!("spec.expirationSeconds: Invalid value: {v}: must be an integer"))
            })?;
            if secs < MIN_TTL_SECS {
                return Err(ApiError::invalid(&format!(
                    "spec.expirationSeconds: Invalid value: {secs}: may not specify a duration less than 10 minutes"
                )));
            }
            if secs > MAX_TTL_SECS {
                return Err(ApiError::invalid(&format!(
                    "spec.expirationSeconds: Invalid value: {secs}: may not specify a duration larger than 2^32 seconds"
                )));
            }
            secs
        }
    };

    let mut kc = KubeClaims {
        namespace: namespace.clone(),
        serviceaccount: Some(ObjectClaim { name: name.clone(), uid: sa_uid }),
        ..Default::default()
    };
    let bound = &spec["boundObjectRef"];
    let mut pod_bound = false;
    if !bound.is_null() {
        let kind = bound["kind"].as_str().unwrap_or("");
        let obj_name = bound["name"].as_str().unwrap_or("");
        if obj_name.is_empty() {
            return Err(ApiError::invalid("spec.boundObjectRef.name: Required value"));
        }
        let (resource, key) = match kind {
            "Pod" => ("pods", ResourceStorage::namespaced_key("pods", &namespace, obj_name)),
            "Secret" => ("secrets", ResourceStorage::namespaced_key("secrets", &namespace, obj_name)),
            "Node" => ("nodes", ResourceStorage::cluster_key("nodes", obj_name)),
            other => {
                return Err(ApiError::bad_request(&format!(
                    "cannot bind token for serviceaccount {name:?} to object of type {other:?}: \
                     only Pod, Secret and Node"
                )))
            }
        };
        let obj = state.storage.get(&key).await?;
        let uid = obj["metadata"]["uid"].as_str().unwrap_or("").to_string();
        if let Some(want) = bound["uid"].as_str().filter(|u| !u.is_empty()) {
            if want != uid {
                return Err(ApiError::conflict(&format!(
                    "the UID in the bound object reference ({want}) does not match the UID in \
                     record ({uid}). The object might have been deleted and then recreated"
                )));
            }
        }
        let claim = ObjectClaim { name: obj_name.to_string(), uid };
        match resource {
            "pods" => {
                let pod_sa = obj["spec"]["serviceAccountName"].as_str().unwrap_or("default");
                if pod_sa != name {
                    return Err(ApiError::bad_request(&format!(
                        "cannot bind token for serviceaccount {name:?} to pod running with \
                         different serviceaccount name"
                    )));
                }
                // The pod's node, for information (upstream 1.30+).
                if let Some(node) = obj["spec"]["nodeName"].as_str().filter(|n| !n.is_empty()) {
                    let uid = state
                        .storage
                        .get(&ResourceStorage::cluster_key("nodes", node))
                        .await
                        .ok()
                        .and_then(|n| n["metadata"]["uid"].as_str().map(String::from))
                        .unwrap_or_default();
                    kc.node = Some(ObjectClaim { name: node.to_string(), uid });
                }
                kc.pod = Some(claim);
                pod_bound = true;
            }
            "secrets" => kc.secret = Some(claim),
            _ => kc.node = Some(claim),
        }
    }

    let now = chrono::Utc::now().timestamp();
    let kube_audiences = audiences.iter().all(|a| keys.api_audiences().contains(a));
    let mut exp = now + ttl;
    if keys.extend_expiration() && pod_bound && ttl == WARN_ONLY_TTL_SECS && kube_audiences {
        kc.warnafter = Some(exp as u64);
        exp = now + EXTENDED_TTL_SECS;
    }

    let claims = Claims {
        iss: Some(keys.issuer().to_string()),
        sub: format!("system:serviceaccount:{namespace}:{name}"),
        aud: Some(Aud::Many(audiences.clone())),
        groups: Vec::new(),
        iat: now as u64,
        nbf: Some(now as u64),
        exp: exp as u64,
        jti: Some(uuid::Uuid::new_v4().to_string()),
        kubernetes: Some(kc),
    };
    let token = keys
        .sign(&claims)
        .ok_or_else(|| ApiError::internal("failed to sign ServiceAccount token"))?;

    let mut out_spec = json!({ "audiences": audiences, "expirationSeconds": ttl });
    if !bound.is_null() {
        out_spec["boundObjectRef"] = bound.clone();
    }
    // The requested lifetime, even when the token was extended: when the
    // holder should refresh it.
    let expires = chrono::DateTime::from_timestamp(now + ttl, 0).unwrap_or_default();
    Ok(Json(json!({
        "kind": "TokenRequest",
        "apiVersion": "authentication.k8s.io/v1",
        "metadata": { "name": name, "namespace": namespace, "creationTimestamp": null },
        "spec": out_spec,
        "status": {
            "token": token,
            "expirationTimestamp": expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }
    })))
}

/// `POST /apis/authentication.k8s.io/v1/tokenreviews`
///
/// Answers "is this token valid, and who is it?" for a component that has been
/// handed one and cannot verify it itself. The kubelet is the caller that
/// matters here: it authenticates inbound requests — `kubectl logs`, `exec`,
/// metrics scrapes — by asking this endpoint, because only the apiserver holds
/// the signing key. stormcos's metadata service asks it which pod a
/// host-network caller is (#182).
///
/// Without it the kubelet cannot validate anything and answers 401 to every
/// authenticated request, including the apiserver's own log proxy. The failure
/// surfaces as a bare "Unauthorized" from `kubectl logs`, which names neither
/// the hop that refused nor the reason (#54).
///
/// An invalid token is **not** an error: the review succeeded and its answer is
/// `authenticated: false`. Returning 401 here would conflate "this caller may
/// not ask" with "the token they asked about is bad".
///
/// `spec.audiences` (none: the apiserver's) must meet the token's `aud`;
/// `status.audiences` is the part that does. A bound token whose object is
/// gone, or was recreated with another uid, is not authenticated; a
/// pod-bound one reports its pod and node in `status.user.extra`.
///
/// A static token from `--token-auth-file` (#188) is answered too, as
/// upstream's TokenReview consults every token authenticator; it is good for
/// the apiserver's audiences only.
pub async fn create_token_review(
    Extension(keys): Extension<SigningKeys>,
    static_tokens: Option<Extension<crate::token_file::StaticTokens>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let token = body["spec"]["token"].as_str().unwrap_or("");
    let wanted: Vec<String> = body["spec"]["audiences"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let static_user = static_tokens.and_then(|t| t.authenticate(token)).and_then(|(username, groups)| {
        let audiences: Vec<String> = if wanted.is_empty() {
            keys.api_audiences().to_vec()
        } else {
            wanted.iter().filter(|a| keys.api_audiences().contains(a)).cloned().collect()
        };
        (!audiences.is_empty()).then(|| crate::auth::TokenUser {
            username,
            uid: String::new(),
            groups,
            extra: Default::default(),
            audiences,
        })
    });
    let user = match static_user {
        Some(u) => Some(u),
        None => keys.authenticate(token, &wanted).await,
    };
    let status = match user {
        Some(u) => {
            let mut user = json!({ "username": u.username, "groups": u.groups });
            if !u.uid.is_empty() {
                user["uid"] = json!(u.uid);
            }
            if !u.extra.is_empty() {
                user["extra"] = json!(u.extra);
            }
            json!({ "authenticated": true, "user": user, "audiences": u.audiences })
        }
        None => json!({
            "authenticated": false,
            "error": "token is invalid, expired, not for these audiences, or its bound object is gone",
        }),
    };

    (
        axum::http::StatusCode::CREATED,
        Json(json!({
            "apiVersion": "authentication.k8s.io/v1",
            "kind": "TokenReview",
            "metadata": {},
            // The token is echoed back by upstream only when it was already
            // present; it is omitted here so a review does not put a live
            // credential into whatever logs the response.
            "spec": { "audiences": body["spec"]["audiences"].clone() },
            "status": status,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tests::test_keys;
    use crate::crd::CrdRegistry;
    use crate::test_store::MemStore;
    use axum::response::IntoResponse;
    use std::sync::Arc;

    /// A namespace `n` with ServiceAccount `app` (uid sa-1), its Pod `p`
    /// (uid pod-1) on Node `node-a` (uid node-1), and a Pod `q` of another
    /// ServiceAccount.
    async fn cluster() -> (AppState, SigningKeys) {
        let storage = Arc::new(ResourceStorage::new(Arc::new(MemStore::default())));
        let put = |key: String, obj: Value| {
            let storage = storage.clone();
            async move { storage.create(&key, obj).await.unwrap() }
        };
        put(
            ResourceStorage::namespaced_key("serviceaccounts", "n", "app"),
            json!({"kind": "ServiceAccount", "metadata": {"name": "app", "namespace": "n", "uid": "sa-1"}}),
        )
        .await;
        put(
            ResourceStorage::namespaced_key("pods", "n", "p"),
            json!({"kind": "Pod", "metadata": {"name": "p", "namespace": "n", "uid": "pod-1"},
                   "spec": {"serviceAccountName": "app", "nodeName": "node-a"}}),
        )
        .await;
        put(
            ResourceStorage::namespaced_key("pods", "n", "q"),
            json!({"kind": "Pod", "metadata": {"name": "q", "namespace": "n", "uid": "pod-2"},
                   "spec": {"serviceAccountName": "other"}}),
        )
        .await;
        put(
            ResourceStorage::cluster_key("nodes", "node-a"),
            json!({"kind": "Node", "metadata": {"name": "node-a", "uid": "node-1"}}),
        )
        .await;
        let state = AppState {
            storage: storage.clone(),
            crd_registry: Arc::new(CrdRegistry::new()),
            service_cidr: "10.96.0.0/12".into(),
        };
        (state, test_keys().with_bound_objects(storage))
    }

    async fn request(state: &AppState, keys: &SigningKeys, spec: Value) -> Result<Value, ApiError> {
        let body = serde_json::to_vec(&json!({"kind": "TokenRequest", "spec": spec})).unwrap();
        create_serviceaccount_token(
            State(state.clone()),
            Extension(keys.clone()),
            Path(("n".into(), "app".into())),
            Bytes::from(body),
        )
        .await
        .map(|Json(v)| v)
    }

    fn claims(token: &str) -> Value {
        use base64::Engine;
        let payload = token.split('.').nth(1).unwrap();
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn review(keys: &SigningKeys, token: &str, audiences: Value) -> Value {
        let resp = create_token_review(
            Extension(keys.clone()),
            None,
            Json(json!({"spec": {"token": token, "audiences": audiences}})),
        )
        .await
        .into_response();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        serde_json::from_slice::<Value>(&body).unwrap()["status"].clone()
    }

    fn pod_ref() -> Value {
        json!({"kind": "Pod", "apiVersion": "v1", "name": "p", "uid": "pod-1"})
    }

    #[tokio::test]
    async fn a_pod_bound_token_carries_upstreams_claims() {
        let (state, keys) = cluster().await;
        let out = request(&state, &keys, json!({"expirationSeconds": 1200, "boundObjectRef": pod_ref()}))
            .await
            .unwrap();
        let c = claims(out["status"]["token"].as_str().unwrap());
        assert_eq!(c["iss"], "https://kubernetes.default.svc");
        assert_eq!(c["sub"], "system:serviceaccount:n:app");
        assert_eq!(c["aud"], json!(["https://kubernetes.default.svc"]));
        assert_eq!(c["exp"].as_i64().unwrap() - c["iat"].as_i64().unwrap(), 1200);
        assert_eq!(c["nbf"], c["iat"]);
        assert!(c["jti"].as_str().is_some_and(|j| !j.is_empty()));
        assert_eq!(
            c["kubernetes.io"],
            json!({"namespace": "n",
                   "pod": {"name": "p", "uid": "pod-1"},
                   "node": {"name": "node-a", "uid": "node-1"},
                   "serviceaccount": {"name": "app", "uid": "sa-1"}})
        );
        assert!(c.get("groups").is_none(), "{c}");
        assert_eq!(out["spec"]["expirationSeconds"], 1200);
        assert_eq!(out["spec"]["boundObjectRef"]["name"], "p");
    }

    #[tokio::test]
    async fn the_review_reports_the_pod_and_its_node() {
        let (state, keys) = cluster().await;
        let out = request(&state, &keys, json!({"boundObjectRef": pod_ref()})).await.unwrap();
        let token = out["status"]["token"].as_str().unwrap();
        let st = review(&keys, token, json!([])).await;
        assert_eq!(st["authenticated"], true, "{st}");
        assert_eq!(st["user"]["username"], "system:serviceaccount:n:app");
        assert_eq!(st["user"]["uid"], "sa-1");
        assert_eq!(st["user"]["groups"], json!(["system:serviceaccounts", "system:serviceaccounts:n"]));
        let extra = &st["user"]["extra"];
        assert_eq!(extra["authentication.kubernetes.io/pod-name"], json!(["p"]));
        assert_eq!(extra["authentication.kubernetes.io/pod-uid"], json!(["pod-1"]));
        assert_eq!(extra["authentication.kubernetes.io/node-name"], json!(["node-a"]));
        assert_eq!(extra["authentication.kubernetes.io/node-uid"], json!(["node-1"]));
        let jti = claims(token)["jti"].as_str().unwrap().to_string();
        assert_eq!(extra["authentication.kubernetes.io/credential-id"], json!([format!("JTI={jti}")]));
        assert_eq!(st["audiences"], json!(["https://kubernetes.default.svc"]));
    }

    #[tokio::test]
    async fn audiences_are_honoured_both_ways() {
        let (state, keys) = cluster().await;
        let out = request(&state, &keys, json!({"audiences": ["vault"], "boundObjectRef": pod_ref()}))
            .await
            .unwrap();
        let token = out["status"]["token"].as_str().unwrap();
        assert_eq!(claims(token)["aud"], json!(["vault"]));
        // For vault, not for the apiserver.
        assert_eq!(review(&keys, token, json!(["vault", "x"])).await["audiences"], json!(["vault"]));
        assert_eq!(review(&keys, token, json!([])).await["authenticated"], false);
        assert!(keys.authenticate(token, &[]).await.is_none());
        // An apiserver token is not good for vault.
        let out = request(&state, &keys, json!({})).await.unwrap();
        let token = out["status"]["token"].as_str().unwrap();
        assert_eq!(review(&keys, token, json!(["vault"])).await["authenticated"], false);
        assert!(keys.authenticate(token, &[]).await.is_some());
    }

    #[tokio::test]
    async fn expiration_is_honoured_and_clamped() {
        let (state, keys) = cluster().await;
        let ttl = |v: &Value| {
            let c = claims(v["status"]["token"].as_str().unwrap());
            c["exp"].as_i64().unwrap() - c["iat"].as_i64().unwrap()
        };
        // Unset: 24 h, until the kubelet refreshes (rustkube-node#122).
        assert_eq!(ttl(&request(&state, &keys, json!({})).await.unwrap()), 86_400);
        assert_eq!(ttl(&request(&state, &keys, json!({"expirationSeconds": 600})).await.unwrap()), 600);
        let e = request(&state, &keys, json!({"expirationSeconds": 599})).await.unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        let e = request(&state, &keys, json!({"expirationSeconds": (1i64 << 32) + 1})).await.unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        // An unbound 3607 s token is not extended.
        assert_eq!(ttl(&request(&state, &keys, json!({"expirationSeconds": 3607})).await.unwrap()), 3607);
    }

    #[tokio::test]
    async fn a_projected_volume_token_is_extended_with_warnafter() {
        let (state, keys) = cluster().await;
        let out = request(&state, &keys, json!({"expirationSeconds": 3607, "boundObjectRef": pod_ref()}))
            .await
            .unwrap();
        let c = claims(out["status"]["token"].as_str().unwrap());
        let iat = c["iat"].as_i64().unwrap();
        assert_eq!(c["exp"].as_i64().unwrap() - iat, 365 * 86_400);
        assert_eq!(c["kubernetes.io"]["warnafter"].as_i64().unwrap() - iat, 3607);
        // The response says when to refresh, not when it dies.
        let exp = chrono::DateTime::parse_from_rfc3339(out["status"]["expirationTimestamp"].as_str().unwrap())
            .unwrap()
            .timestamp();
        assert_eq!(exp - iat, 3607);
        // Off, or for another audience: as asked.
        let off = keys.clone().with_token_config("https://kubernetes.default.svc", &[], false);
        let out = request(&state, &off, json!({"expirationSeconds": 3607, "boundObjectRef": pod_ref()}))
            .await
            .unwrap();
        let c = claims(out["status"]["token"].as_str().unwrap());
        assert_eq!(c["exp"].as_i64().unwrap() - c["iat"].as_i64().unwrap(), 3607);
        let out = request(
            &state,
            &keys,
            json!({"expirationSeconds": 3607, "audiences": ["vault"], "boundObjectRef": pod_ref()}),
        )
        .await
        .unwrap();
        let c = claims(out["status"]["token"].as_str().unwrap());
        assert_eq!(c["exp"].as_i64().unwrap() - c["iat"].as_i64().unwrap(), 3607);
    }

    #[tokio::test]
    async fn binding_is_checked_at_request() {
        let (state, keys) = cluster().await;
        let wrong_uid = json!({"kind": "Pod", "name": "p", "uid": "pod-9"});
        let e = request(&state, &keys, json!({"boundObjectRef": wrong_uid})).await.unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::CONFLICT);
        let missing = json!({"kind": "Pod", "name": "nope"});
        let e = request(&state, &keys, json!({"boundObjectRef": missing})).await.unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::NOT_FOUND);
        let other_sa = json!({"kind": "Pod", "name": "q"});
        let e = request(&state, &keys, json!({"boundObjectRef": other_sa})).await.unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::BAD_REQUEST);
        let bad_kind = json!({"kind": "ConfigMap", "name": "p"});
        let e = request(&state, &keys, json!({"boundObjectRef": bad_kind})).await.unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::BAD_REQUEST);
        // No uid given: the current object's.
        let out = request(&state, &keys, json!({"boundObjectRef": {"kind": "Node", "name": "node-a"}}))
            .await
            .unwrap();
        let c = claims(out["status"]["token"].as_str().unwrap());
        assert_eq!(c["kubernetes.io"]["node"], json!({"name": "node-a", "uid": "node-1"}));
        assert!(c["kubernetes.io"].get("pod").is_none());
    }

    #[tokio::test]
    async fn a_token_dies_with_its_pod() {
        let (state, keys) = cluster().await;
        let out = request(&state, &keys, json!({"boundObjectRef": pod_ref()})).await.unwrap();
        let token = out["status"]["token"].as_str().unwrap().to_string();
        assert!(keys.authenticate(&token, &[]).await.is_some());
        let key = ResourceStorage::namespaced_key("pods", "n", "p");

        // Terminating: good for a minute past its deletionTimestamp.
        let mut pod = state.storage.get(&key).await.unwrap();
        pod["metadata"]["deletionTimestamp"] = json!(chrono::Utc::now().to_rfc3339());
        state.storage.update(&key, pod.clone(), None).await.unwrap();
        assert!(keys.authenticate(&token, &[]).await.is_some());
        let past = chrono::Utc::now() - chrono::Duration::seconds(120);
        pod["metadata"]["deletionTimestamp"] = json!(past.to_rfc3339());
        state.storage.update(&key, pod, None).await.unwrap();
        assert!(keys.authenticate(&token, &[]).await.is_none());

        // Gone, then recreated under the same name: a new pod, not this one.
        state.storage.delete(&key, None).await.unwrap();
        assert!(keys.authenticate(&token, &[]).await.is_none());
        state
            .storage
            .create(
                &key,
                json!({"kind": "Pod", "metadata": {"name": "p", "namespace": "n", "uid": "pod-3"},
                       "spec": {"serviceAccountName": "app"}}),
            )
            .await
            .unwrap();
        assert!(keys.authenticate(&token, &[]).await.is_none());
        assert_eq!(review(&keys, &token, json!([])).await["authenticated"], false);
    }

    #[tokio::test]
    async fn a_token_dies_with_its_serviceaccount() {
        let (state, keys) = cluster().await;
        let out = request(&state, &keys, json!({})).await.unwrap();
        let token = out["status"]["token"].as_str().unwrap().to_string();
        assert!(keys.authenticate(&token, &[]).await.is_some());
        let key = ResourceStorage::namespaced_key("serviceaccounts", "n", "app");
        state.storage.delete(&key, None).await.unwrap();
        state
            .storage
            .create(&key, json!({"kind": "ServiceAccount", "metadata": {"name": "app", "namespace": "n", "uid": "sa-2"}}))
            .await
            .unwrap();
        assert!(keys.authenticate(&token, &[]).await.is_none());
    }

    #[tokio::test]
    async fn a_bound_token_without_a_store_to_check_is_refused() {
        let (state, keys) = cluster().await;
        let out = request(&state, &keys, json!({"boundObjectRef": pod_ref()})).await.unwrap();
        let token = out["status"]["token"].as_str().unwrap();
        assert!(test_keys().authenticate(token, &[]).await.is_none());
    }
}
