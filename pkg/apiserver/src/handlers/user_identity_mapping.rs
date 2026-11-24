//! `user.openshift.io/v1` UserIdentityMappings (#390): create, get, update
//! and delete, no list or watch, as OpenShift serves them. The mapping is
//! not stored; see [`crate::user_identities`].

use crate::error::ApiError;
use crate::handlers::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::Value;

/// POST `useridentitymappings`.
pub async fn create(State(state): State<AppState>, Json(body): Json<Value>) -> Result<impl IntoResponse, ApiError> {
    let dry_run = crate::admission::dry_run();
    let (mapping, _) = crate::user_identities::create_or_update(&state.storage, body, true, dry_run).await?;
    Ok((StatusCode::CREATED, Json(mapping)))
}

/// GET `useridentitymappings/{name}`; a Table when asked (`oc get`).
pub async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let mapping = crate::user_identities::get_mapping(&state.storage, &name).await?;
    if crate::table::wants_table(&headers) {
        return Ok(Json(crate::table::to_table(crate::user_identities::MAPPINGS, mapping)));
    }
    Ok(Json(mapping))
}

/// PUT `useridentitymappings/{name}`: maps the Identity to the User the body
/// names, creating the mapping when there is none.
pub async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(mut body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    if !body.is_object() {
        return Err(ApiError::bad_request("a UserIdentityMapping must be an object"));
    }
    match body["metadata"]["name"].as_str().filter(|n| !n.is_empty()) {
        Some(n) if n != name => {
            return Err(ApiError::bad_request(&format!(
                "the name of the object ({n}) does not match the name on the URL ({name})"
            )));
        }
        Some(_) => {}
        None => {
            if !body["metadata"].is_object() {
                body["metadata"] = serde_json::json!({});
            }
            body["metadata"]["name"] = Value::String(name);
        }
    }
    let dry_run = crate::admission::dry_run();
    let (mapping, created) = crate::user_identities::create_or_update(&state.storage, body, false, dry_run).await?;
    Ok((if created { StatusCode::CREATED } else { StatusCode::OK }, Json(mapping)))
}

/// DELETE `useridentitymappings/{name}`: unmaps the Identity; both objects stay.
pub async fn delete(State(state): State<AppState>, Path(name): Path<String>) -> Result<impl IntoResponse, ApiError> {
    let dry_run = crate::admission::dry_run();
    Ok(Json(crate::user_identities::delete_mapping(&state.storage, &name, dry_run).await?))
}
