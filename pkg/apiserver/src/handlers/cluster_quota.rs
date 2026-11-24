//! `quota.openshift.io/v1` AppliedClusterResourceQuotas (#391): the
//! ClusterResourceQuotas selecting a namespace, read in it. See
//! [`crate::cluster_quota`].

use crate::error::ApiError;
use crate::handlers::AppState;
use axum::extract::{Path, RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::Json;

/// LIST `namespaces/{namespace}/appliedclusterresourcequotas`.
pub async fn list_applied(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let list = crate::cluster_quota::list_applied(&state.storage, &namespace, query.as_deref().unwrap_or("")).await?;
    if crate::table::wants_table(&headers) {
        return Ok(Json(crate::table::to_table(crate::cluster_quota::APPLIED, list)).into_response());
    }
    Ok(Json(list).into_response())
}

/// GET `namespaces/{namespace}/appliedclusterresourcequotas/{name}`.
pub async fn get_applied(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let q = crate::cluster_quota::get_applied(&state.storage, &namespace, &name).await?;
    if crate::table::wants_table(&headers) {
        return Ok(Json(crate::table::to_table(crate::cluster_quota::APPLIED, q)).into_response());
    }
    Ok(Json(q).into_response())
}
