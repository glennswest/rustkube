//! The `apps.openshift.io/v1` subresources of a DeploymentConfig (#375):
//! `instantiate` (`oc rollout latest`), `rollback` (`oc rollback`,
//! `oc rollout undo`), `log` (`oc logs dc/…`) and `scale` (`oc scale dc/…`).
//! The objects' semantics are `crate::deployment_configs`.

use crate::deployment_configs as dcs;
use crate::error::ApiError;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;
use std::collections::HashMap;

/// `POST …/deploymentconfigs/{name}/instantiate`
pub async fn instantiate(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    if body["name"].as_str().is_some_and(|n| !n.is_empty() && n != name) {
        return Err(ApiError::bad_request(&format!(
            "the name of the object ({}) does not match the name on the URL ({name})",
            body["name"].as_str().unwrap_or("")
        )));
    }
    match dcs::instantiate(&state.storage, &namespace, &name, &body).await? {
        Some(dc) => Ok((StatusCode::CREATED, Json(dc)).into_response()),
        // "cannot be instantiated": OpenShift's Status of code 204.
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// `POST …/deploymentconfigs/{name}/rollback`
pub async fn rollback(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let out = dcs::rollback(&state.storage, &namespace, &name, &body).await?;
    Ok((StatusCode::CREATED, Json(out)).into_response())
}

fn truthy(params: &HashMap<String, String>, k: &str) -> bool {
    params.get(k).is_some_and(|v| v == "true" || v == "1")
}

/// `GET …/deploymentconfigs/{name}/log`: the rollout's log, which the
/// controller that runs it records on the version's ReplicationController
/// (#375: no deployer pod). `version` names one, `previous` the one before;
/// `follow` streams it until the rollout ends. A Custom version (#392) has a
/// deployer pod: while it is there, its log is served, as upstream's
/// `LogREST` does; after (or when its node cannot serve it), the copy kept
/// on the version.
pub async fn log(
    State(state): State<AppState>,
    Extension(keys): Extension<crate::auth::SigningKeys>,
    Path((namespace, name)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let previous = truthy(&params, "previous");
    let version_asked = match params.get("version") {
        Some(v) => {
            let n = v.parse::<i64>().unwrap_or(0);
            let mut errs = Vec::new();
            if n <= 0 {
                errs.push(format!("version: Invalid value: {n}: deployment version must be greater than 0"));
            }
            if previous {
                errs.push("previous: Invalid value: true: cannot use previous when a version is specified".into());
            }
            if !errs.is_empty() {
                let what = if errs.len() == 1 { errs[0].clone() } else { format!("[{}]", errs.join(", ")) };
                return Err(ApiError::invalid(&format!("DeploymentLogOptions.apps.openshift.io \"\" is invalid: {what}")));
            }
            Some(n)
        }
        None => None,
    };
    let dc = state
        .storage
        .get(&ResourceStorage::namespaced_key(dcs::RESOURCE, &namespace, &name))
        .await
        .map_err(|_| ApiError::not_found("deploymentconfigs.apps.openshift.io", &name))?;
    let latest = dc["status"]["latestVersion"].as_i64().unwrap_or(0);
    let version = version_asked.unwrap_or(if previous { latest - 1 } else { latest });
    if latest == 0 {
        return Err(ApiError::bad_request(&format!("no deployment exists for deploymentConfig {name:?}")));
    }
    if version <= 0 || version > latest {
        return Err(ApiError::bad_request(&format!(
            "deployment config {name:?} has no version {version} (latest is {latest})"
        )));
    }
    let rc_name = dcs::rc_name(&name, version);
    let key = ResourceStorage::namespaced_key("replicationcontrollers", &namespace, &rc_name);
    let rc = state
        .storage
        .get(&key)
        .await
        .map_err(|_| ApiError::bad_request(&format!("replicationcontrollers {rc_name:?} not found")))?;
    if dcs::decode(&rc).is_some_and(|d| d["spec"]["strategy"]["type"] == "Custom") {
        let pod_name = crate::builds::get_name(&rc_name, "deploy", 63);
        if state.storage.get(&ResourceStorage::namespaced_key("pods", &namespace, &pod_name)).await.is_ok() {
            let mut q: HashMap<String, String> = HashMap::new();
            for k in ["timestamps", "tailLines", "sinceSeconds", "sinceTime", "limitBytes", "follow"] {
                if let Some(v) = params.get(k) {
                    q.insert(k.into(), v.clone());
                }
            }
            q.insert("container".into(), "deployment".into());
            let resp = crate::handlers::logs::pod_logs(State(state.clone()), Extension(keys), Path((namespace.clone(), pod_name)),
                                                        Query(q)).await;
            // A pod whose node cannot be reached: the copy below instead.
            if resp.status().is_success() {
                return Ok(resp);
            }
        }
    }
    let phase = |rc: &Value| rc["metadata"]["annotations"][dcs::PHASE_ANNOTATION].as_str().unwrap_or("").to_string();
    let text = |rc: &Value| rc["metadata"]["annotations"][dcs::LOG_ANNOTATION].as_str().unwrap_or("").to_string();
    let terminal = |p: &str| p == "Complete" || p == "Failed";
    if !truthy(&params, "follow") || terminal(&phase(&rc)) {
        return Ok(plain(text(&rc)));
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(16);
    let storage = state.storage.clone();
    tokio::spawn(async move {
        let mut sent = 0usize;
        loop {
            let Ok(rc) = storage.get(&key).await else { return };
            let t = text(&rc);
            // The record keeps its tail when it grows past its bound; a
            // shorter text is a fresh one (a retry), sent whole.
            if t.len() < sent {
                sent = 0;
            }
            if t.len() > sent {
                if tx.send(Ok(t[sent..].to_string().into())).await.is_err() {
                    return;
                }
                sent = t.len();
            }
            if terminal(&phase(&rc)) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
    let body = axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain")
        .body(body)
        .unwrap_or_else(|e| ApiError::internal(&format!("building log response: {e}")).into_response()))
}

fn plain(text: String) -> Response {
    ([(axum::http::header::CONTENT_TYPE, "text/plain")], text).into_response()
}

fn dc_key(namespace: &str, name: &str) -> String {
    ResourceStorage::namespaced_key(dcs::RESOURCE, namespace, name)
}

/// `GET …/deploymentconfigs/{name}/scale`
pub async fn get_scale(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    crate::handlers::scale::get_builtin(&state, &dc_key(&namespace, &name)).await
}

/// `PUT …/deploymentconfigs/{name}/scale`
pub async fn put_scale(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    crate::handlers::scale::put_builtin(&state, &dc_key(&namespace, &name), &body).await
}

/// `PATCH …/deploymentconfigs/{name}/scale`
pub async fn patch_scale(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    crate::handlers::scale::patch_builtin(&state, &dc_key(&namespace, &name), &headers, &body).await
}
