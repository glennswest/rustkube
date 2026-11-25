//! The `build.openshift.io/v1` subresources (#374): `buildconfigs/instantiate`
//! (`oc start-build`), `builds/clone` (`oc start-build --from-build`),
//! `builds/details` (the builder's own update) and `builds/log`
//! (`oc logs build/…`, `oc logs bc/…`, `oc start-build --follow`).
//! The objects' semantics are `crate::builds`.

use crate::auth::UserInfo;
use crate::builds;
use crate::error::ApiError;
use crate::handlers::AppState;
use crate::rbac_engine::RbacEngine;
use crate::storage::ResourceStorage;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// OpenShift's `BuildTriggerCauseManualMsg`.
const MANUAL: &str = "Manually triggered";
/// `apiserverbuildutil.NoBuildLogsMessage`.
const NO_LOGS: &str = "No logs are available.";
/// The builder containers, in OpenShift's order (`BuildContainerNames`).
const BUILD_CONTAINERS: [&str; 3] = ["custom-build", "sti-build", "docker-build"];
/// How long a log request waits for a New or Pending build to run.
const WAIT_FOR_RUNNING: std::time::Duration = std::time::Duration::from_secs(30);

/// A BuildRequest body: its name must be the URL's, and it gets OpenShift's
/// default trigger cause.
fn request_of(mut body: Value, name: &str) -> Result<Value, ApiError> {
    if !body.is_object() {
        return Err(ApiError::bad_request("the body of the request was not a BuildRequest"));
    }
    let named = body["metadata"]["name"].as_str().unwrap_or("").to_string();
    if !named.is_empty() && named != name {
        return Err(ApiError::bad_request(&format!(
            "the name of the object ({named}) does not match the name on the URL ({name})"
        )));
    }
    if body["triggeredBy"].as_array().is_none_or(|t| t.is_empty()) {
        body["triggeredBy"] = json!([{"message": MANUAL}]);
    }
    Ok(body)
}

fn created(build: Value) -> Response {
    (StatusCode::CREATED, Json(build)).into_response()
}

/// `POST …/buildconfigs/{name}/instantiate`
pub async fn instantiate(
    State(state): State<AppState>,
    Extension(user): Extension<UserInfo>,
    rbac: Option<Extension<Arc<RbacEngine>>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let req = request_of(body, &name)?;
    let rbac = rbac.as_ref().map(|Extension(r)| r.as_ref());
    let get = |path: String| crate::image_policy::fetch(&state, path);
    let build = builds::instantiate(&state.storage, &get, rbac, &user, &namespace, &name, &req).await?;
    Ok(created(build))
}

/// `POST …/buildconfigs/{name}/webhooks/{secret}/{type}` (#386): a build
/// from a GitHub, GitLab, Bitbucket or generic webhook delivery. 200 with
/// the Build, or a Success Status when the delivery asks for none (a ping,
/// another branch).
pub async fn webhook(
    State(state): State<AppState>,
    Path((namespace, name, secret, hook_type)): Path<(String, String, String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let get = |path: String| crate::image_policy::fetch(&state, path);
    match builds::webhook(&state.storage, &get, &namespace, &name, &secret, &hook_type, &headers, &body).await? {
        Some(build) => Ok((StatusCode::OK, Json(build)).into_response()),
        None => Ok((StatusCode::OK, Json(json!({"kind": "Status", "apiVersion": "v1", "metadata": {},
                                                 "status": "Success", "code": 200,
                                                 "message": "no build was started for this delivery"}))).into_response()),
    }
}

/// `POST …/builds/{name}/clone`
pub async fn clone(
    State(state): State<AppState>,
    Extension(user): Extension<UserInfo>,
    rbac: Option<Extension<Arc<RbacEngine>>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let req = request_of(body, &name)?;
    let rbac = rbac.as_ref().map(|Extension(r)| r.as_ref());
    let build = builds::clone(&state.storage, rbac, &user, &namespace, &name, &req).await?;
    Ok(created(build))
}

/// `PUT …/builds/{name}/details`: unconditional, as OpenShift's.
pub async fn details(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let key = ResourceStorage::namespaced_key(builds::BUILDS, &namespace, &name);
    for _ in 0..3 {
        let stored = get_build(&state.storage, &namespace, &name).await?;
        let out = builds::details(&body, &stored)?;
        let rev = stored["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
        match state.storage.update(&key, out, rev).await {
            Ok(v) => return Ok(Json(v).into_response()),
            Err(e) if e.reason == "Conflict" => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ApiError::conflict(&format!(
        "Operation cannot be fulfilled on builds.build.openshift.io {name:?}: the object has been modified; please apply your changes to the latest version and try again"
    )))
}

async fn get_build(storage: &ResourceStorage, ns: &str, name: &str) -> Result<Value, ApiError> {
    storage
        .get(&ResourceStorage::namespaced_key(builds::BUILDS, ns, name))
        .await
        .map_err(|e| if e.status == StatusCode::NOT_FOUND { ApiError::not_found("builds.build.openshift.io", name) } else { e })
}

fn truthy(params: &HashMap<String, String>, k: &str) -> bool {
    params.get(k).is_some_and(|v| v == "true" || v == "1")
}

/// `GET …/builds/{name}/log`, as OpenShift's `buildlog` REST: the build's
/// init containers' logs one after another, then its builder container's,
/// in one stream; waiting up to 30 s for a build that has not started.
pub async fn log(
    State(state): State<AppState>,
    Extension(keys): Extension<crate::auth::SigningKeys>,
    Path((namespace, name)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let previous = truthy(&params, "previous");
    let follow = truthy(&params, "follow");
    if let Some(v) = params.get("version") {
        let mut errs = Vec::new();
        match v.parse::<i64>() {
            Ok(n) if n <= 0 => errs.push(format!("version: Invalid value: {n}: build version must be greater than 0")),
            Err(_) => errs.push(format!("version: Invalid value: {v:?}: build version must be greater than 0")),
            _ => {}
        }
        if previous {
            errs.push("previous: Invalid value: true: cannot use previous when a version is specified".into());
        }
        if !errs.is_empty() {
            let what = if errs.len() == 1 { errs[0].clone() } else { format!("[{}]", errs.join(", ")) };
            return Err(ApiError::invalid(&format!("BuildLogOptions.build.openshift.io \"\" is invalid: {what}")));
        }
    }
    let mut build = get_build(&state.storage, &namespace, &name).await?;
    if previous {
        let version = build["metadata"]["annotations"][builds::BUILD_NUMBER_ANNOTATION]
            .as_str()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        let config = build["metadata"]["annotations"][builds::BUILD_CONFIG_ANNOTATION]
            .as_str()
            .or(build["status"]["config"]["name"].as_str())
            .unwrap_or("")
            .to_string();
        build = get_build(&state.storage, &namespace, &format!("{config}-{}", version - 1)).await?;
    }
    let build_name = build["metadata"]["name"].as_str().unwrap_or("").to_string();
    match build["status"]["phase"].as_str().unwrap_or("") {
        "New" | "Pending" => {
            if truthy(&params, "nowait") {
                return Ok((StatusCode::OK, "").into_response());
            }
            let deadline = tokio::time::Instant::now() + WAIT_FOR_RUNNING;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let latest = get_build(&state.storage, &namespace, &build_name).await?;
                match latest["status"]["phase"].as_str().unwrap_or("") {
                    "New" | "Pending" if tokio::time::Instant::now() < deadline => continue,
                    "New" | "Pending" => {
                        return Err(ApiError {
                            status: StatusCode::GATEWAY_TIMEOUT,
                            reason: "Timeout".into(),
                            message: format!(
                                "timed out waiting for build {build_name} to start after {}s",
                                WAIT_FOR_RUNNING.as_secs()
                            ),
                            continue_token: None,
                            details: Some(json!({"retryAfterSeconds": 1})),
                        })
                    }
                    "Error" => {
                        return Err(ApiError::bad_request(&format!(
                            "unable to wait for build {build_name} to run: build {build_name} encountered an error: {NO_LOGS}"
                        )))
                    }
                    "Cancelled" => {
                        return Err(ApiError::bad_request(&format!(
                            "unable to wait for build {build_name} to run: build {build_name} was cancelled: {NO_LOGS}"
                        )))
                    }
                    _ => break,
                }
            }
        }
        "Cancelled" => {
            return Err(ApiError::bad_request(&format!("build {build_name} was cancelled. {NO_LOGS}")));
        }
        "Error" => {
            return Err(ApiError::bad_request(&format!("build {build_name} is in an error state. {NO_LOGS}")));
        }
        _ => {}
    }
    let pod_name = build["metadata"]["annotations"][builds::BUILD_POD_NAME_ANNOTATION]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| builds::build_pod_name(&build_name));
    let pod_key = ResourceStorage::namespaced_key("pods", &namespace, &pod_name);
    let pod = state
        .storage
        .get(&pod_key)
        .await
        .map_err(|_| ApiError::bad_request(&format!("pods {pod_name:?} not found")))?;

    // Options passed through to each container's log.
    let mut passed: HashMap<String, String> = HashMap::new();
    for k in ["timestamps", "tailLines", "sinceSeconds", "sinceTime", "limitBytes", "follow"] {
        if let Some(v) = params.get(k) {
            passed.insert(k.into(), v.clone());
        }
    }
    let pod_log = {
        let state = state.clone();
        let keys = keys.clone();
        let namespace = namespace.clone();
        let pod_name = pod_name.clone();
        move |container: String, follow: bool| {
            let mut q = passed.clone();
            q.insert("container".into(), container);
            if follow {
                q.insert("follow".into(), "true".into());
            } else {
                q.remove("follow");
            }
            let (state, keys, namespace, pod_name) = (state.clone(), keys.clone(), namespace.clone(), pod_name.clone());
            async move {
                crate::handlers::logs::pod_logs(State(state), Extension(keys), Path((namespace, pod_name)), Query(q)).await
            }
        }
    };

    let init: Vec<String> = pod["spec"]["initContainers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect();
    let main = pod["spec"]["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["name"].as_str())
        .find(|n| BUILD_CONTAINERS.contains(n))
        .unwrap_or("")
        .to_string();
    // An old-style build pod (no init containers): the pod's own log.
    if init.is_empty() {
        let main = if main.is_empty() {
            pod["spec"]["containers"][0]["name"].as_str().unwrap_or("").to_string()
        } else {
            main
        };
        return Ok(pod_log(main, follow).await);
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(16);
    let storage = state.storage.clone();
    tokio::spawn(async move {
        use futures::StreamExt;
        let pipe = |resp: Response, tx: tokio::sync::mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>| async move {
            if !resp.status().is_success() {
                return false;
            }
            let mut body = resp.into_body().into_data_stream();
            while let Some(chunk) = body.next().await {
                match chunk {
                    Ok(b) => {
                        if tx.send(Ok(b)).await.is_err() {
                            return false;
                        }
                    }
                    Err(_) => return false,
                }
            }
            true
        };
        let mut done: Vec<String> = Vec::new();
        let mut init_failed = false;
        loop {
            let Ok(pod) = storage.get(&pod_key).await else {
                let _ = tx
                    .send(Ok(format!("error retrieving build pod {namespace}/{pod_name} : not found").into()))
                    .await;
                return;
            };
            let mut waiting = false;
            let mut progressed = false;
            for st in pod["status"]["initContainerStatuses"].as_array().into_iter().flatten() {
                let cname = st["name"].as_str().unwrap_or("").to_string();
                let terminated = &st["state"]["terminated"];
                if terminated.is_object() && terminated["exitCode"].as_i64().unwrap_or(0) != 0 {
                    init_failed = true;
                    if done.contains(&cname) {
                        break;
                    }
                }
                if done.contains(&cname) {
                    continue;
                }
                if st["state"]["waiting"].is_object() {
                    waiting = true;
                    continue;
                }
                let resp = pod_log(cname.clone(), follow && !terminated.is_object()).await;
                if !pipe(resp, tx.clone()).await {
                    return;
                }
                progressed = true;
                done.push(cname);
                if init_failed {
                    break;
                }
            }
            if !follow || init_failed || !waiting {
                break;
            }
            if !progressed {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        if init_failed {
            return;
        }
        // The builder container, once the pod has left Pending.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
        let phase = loop {
            let Ok(pod) = storage.get(&pod_key).await else {
                let _ = tx
                    .send(Ok(format!(
                        "error while getting build logs, could not retrieve build pod {namespace}/{pod_name} : not found"
                    )
                    .into()))
                    .await;
                return;
            };
            let phase = pod["status"]["phase"].as_str().unwrap_or("Pending").to_string();
            if phase != "Pending" || tokio::time::Instant::now() >= deadline {
                break phase;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        };
        if phase == "Pending" || main.is_empty() {
            return;
        }
        let ended = phase == "Failed" || phase == "Succeeded";
        let resp = pod_log(main, follow && !ended).await;
        pipe(resp, tx).await;
    });
    let body = axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain")
        .body(body)
        .unwrap_or_else(|e| ApiError::internal(&format!("building log response: {e}")).into_response()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_request_names_its_url_and_says_why() {
        let req = request_of(json!({"kind": "BuildRequest", "metadata": {"name": "web"}}), "web").unwrap();
        assert_eq!(req["triggeredBy"], json!([{"message": "Manually triggered"}]));
        let kept = request_of(json!({"metadata": {"name": "web"}, "triggeredBy": [{"message": "x"}]}), "web").unwrap();
        assert_eq!(kept["triggeredBy"][0]["message"], "x");
        let e = request_of(json!({"metadata": {"name": "other"}}), "web").unwrap_err();
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert!(e.message.contains("does not match the name on the URL"));
    }
}
