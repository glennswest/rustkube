//! ServiceAccount TokenRequest — mints bound JWT tokens.
//!
//! `POST /api/v1/namespaces/{ns}/serviceaccounts/{name}/token` issues a signed
//! token whose subject is `system:serviceaccount:{ns}:{name}` and whose groups
//! are `system:serviceaccounts` and `system:serviceaccounts:{ns}` — the identity
//! the RBAC engine and auth middleware already understand.

use crate::auth::SigningKeys;
use crate::error::ApiError;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde_json::{json, Value};

/// Default token lifetime (matches `SigningKeys::create_token`).
const TOKEN_TTL_SECS: i64 = 86_400;

pub async fn create_serviceaccount_token(
    State(state): State<AppState>,
    Extension(keys): Extension<SigningKeys>,
    Path((namespace, name)): Path<(String, String)>,
    // Accept any body/content-type (real clients POST a TokenRequest; we default
    // the lifetime rather than parse spec.expirationSeconds for now).
    _body: Bytes,
) -> Result<Json<Value>, ApiError> {
    // The ServiceAccount must exist (404 otherwise).
    let sa_key = ResourceStorage::namespaced_key("serviceaccounts", &namespace, &name);
    state.storage.get(&sa_key).await?;

    let sub = format!("system:serviceaccount:{namespace}:{name}");
    let groups = vec![
        "system:serviceaccounts".to_string(),
        format!("system:serviceaccounts:{namespace}"),
    ];
    let token = keys
        .create_token(&sub, &groups)
        .ok_or_else(|| ApiError::internal("failed to sign ServiceAccount token"))?;

    let exp = chrono::Utc::now() + chrono::Duration::seconds(TOKEN_TTL_SECS);
    Ok(Json(json!({
        "kind": "TokenRequest",
        "apiVersion": "authentication.k8s.io/v1",
        "metadata": { "name": name, "namespace": namespace, "creationTimestamp": null },
        "status": {
            "token": token,
            "expirationTimestamp": exp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }
    })))
}

/// `POST /apis/authentication.k8s.io/v1/tokenreviews`
///
/// Answers "is this token valid, and who is it?" for a component that has been
/// handed one and cannot verify it itself. The kubelet is the caller that
/// matters here: it authenticates inbound requests — `kubectl logs`, `exec`,
/// metrics scrapes — by asking this endpoint, because only the apiserver holds
/// the signing key.
///
/// Without it the kubelet cannot validate anything and answers 401 to every
/// authenticated request, including the apiserver's own log proxy. The failure
/// surfaces as a bare "Unauthorized" from `kubectl logs`, which names neither
/// the hop that refused nor the reason (#54).
///
/// An invalid token is **not** an error: the review succeeded and its answer is
/// `authenticated: false`. Returning 401 here would conflate "this caller may
/// not ask" with "the token they asked about is bad".
pub async fn create_token_review(
    axum::extract::Extension(keys): axum::extract::Extension<crate::auth::SigningKeys>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl axum::response::IntoResponse {
    let token = body["spec"]["token"].as_str().unwrap_or("");
    let audiences = body["spec"]["audiences"].clone();

    let status = match keys.validate_token(token) {
        Some(data) => serde_json::json!({
            "authenticated": true,
            "user": {
                "username": data.claims.sub,
                "groups": data.claims.groups,
            },
            "audiences": audiences,
        }),
        None => serde_json::json!({
            "authenticated": false,
            "error": "token is invalid or expired",
        }),
    };

    (
        axum::http::StatusCode::CREATED,
        axum::Json(serde_json::json!({
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
