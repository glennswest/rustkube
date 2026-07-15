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
