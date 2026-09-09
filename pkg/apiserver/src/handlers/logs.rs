//! The `pods/log` subresource.
//!
//! `kubectl logs` does not read files. It issues
//! `GET /api/v1/namespaces/{ns}/pods/{name}/log`, and the apiserver proxies
//! that to the kubelet of the node the pod is on, streaming the answer back.
//! Without this the request 404s with "the server could not find the requested
//! resource (get pods x)" — a message that names the pod rather than the
//! subresource, which sends the reader to look at the pod first (#54).
//!
//! The same proxy path is what `exec`, `attach` and `portforward` will need, so
//! the node-address lookup and the client live here rather than in the handler.

use crate::error::ApiError;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::collections::HashMap;

/// `GET /api/v1/namespaces/{namespace}/pods/{name}/log`
pub async fn pod_logs(
    State(state): State<AppState>,
    Extension(keys): Extension<crate::auth::SigningKeys>,
    Path((namespace, name)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let storage = &state.storage;
    let pod = match storage
        .get(&ResourceStorage::namespaced_key("pods", &namespace, &name))
        .await
    {
        Ok(p) => p,
        Err(_) => return ApiError::not_found("pods", &name).into_response(),
    };

    let container = match pick_container(&pod, params.get("container").map(String::as_str), &name) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };

    let Some(node_name) = pod["spec"]["nodeName"].as_str().filter(|s| !s.is_empty()) else {
        return ApiError {
            status: StatusCode::BAD_REQUEST,
            reason: "BadRequest".into(),
            message: format!("pod {namespace}/{name} is not assigned to a node yet"),
        }
        .into_response();
    };

    let Some(addr) = node_address(storage, node_name).await else {
        return ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            reason: "InternalError".into(),
            message: format!("no usable address for node {node_name}"),
        }
        .into_response();
    };

    // Forward the log options the client asked for, minus the one that named
    // the container — that is in the path on the kubelet side.
    let mut query: Vec<String> = Vec::new();
    for k in [
        "tailLines",
        "sinceSeconds",
        "sinceTime",
        "timestamps",
        "previous",
        "follow",
        "limitBytes",
    ] {
        if let Some(v) = params.get(k) {
            query.push(format!("{k}={v}"));
        }
    }
    let q = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    let url = format!("https://{addr}:10250/containerLogs/{namespace}/{name}/{container}{q}");

    // The kubelet serves a self-signed certificate whose SANs are the node name
    // and IP; upstream verifies it against the cluster CA. Until certificates
    // are issued (rustkube#20) the connection is not verified, which is stated
    // here rather than left for a reader to infer from a builder flag.
    // No overall timeout when following: `-f` is open-ended by design, and a
    // 30-second cap on it would look like the log simply stopping.
    let following = params.get("follow").map(|v| v == "true" || v == "1").unwrap_or(false);
    let mut builder = reqwest::Client::builder().danger_accept_invalid_certs(true);
    if !following {
        builder = builder.timeout(std::time::Duration::from_secs(30));
    }
    let client = match builder.build()
    {
        Ok(c) => c,
        Err(e) => {
            return ApiError::internal(&format!("cannot build kubelet client: {e}")).into_response()
        }
    };

    // Authenticate to the kubelet as the apiserver.
    //
    // The kubelet validates a bearer token by TokenReview against this
    // apiserver, so a token this apiserver signs is one it will accept — which
    // means no shared secret has to be baked into the node image, and the
    // identity in the kubelet's logs is the apiserver rather than "someone
    // with the token". `system:masters` because reading any pod's log on any
    // node is exactly what this endpoint is for.
    //
    // Without it the proxy is answered with 401 and `kubectl logs` reports
    // "Unauthorized" with nothing to say which hop refused.
    let bearer = keys
        .create_token("system:kube-apiserver", &["system:masters".to_string()])
        .unwrap_or_default();

    match client.get(&url).bearer_auth(&bearer).send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            // Streamed, not buffered.
            //
            // `kubectl logs -f` is an open-ended response: reading it to a
            // String waits for an end that never comes, so `follow` hung until
            // the request timed out and then printed everything at once. The
            // body is passed through chunk by chunk instead, which is also
            // what keeps a large log off this process's heap.
            axum::body::Body::from_stream(resp.bytes_stream())
                .pipe_with_status(status)
        }
        Err(e) => ApiError::internal(&format!("reaching kubelet at {addr}: {e}")).into_response(),
    }
}

/// Small helper so the streaming response reads as one expression above.
trait PipeWithStatus {
    fn pipe_with_status(self, status: StatusCode) -> Response;
}

impl PipeWithStatus for axum::body::Body {
    fn pipe_with_status(self, status: StatusCode) -> Response {
        Response::builder()
            .status(status)
            .header("content-type", "text/plain; charset=utf-8")
            .body(self)
            .unwrap_or_else(|e| {
                ApiError::internal(&format!("building log response: {e}")).into_response()
            })
    }
}

/// Which container the caller meant.
///
/// Upstream requires the name when a pod has more than one and lists the
/// choices in the error, which is the difference between a usable message and
/// a puzzle.
///
/// A named container may be an init or an ephemeral one. That is not a
/// nicety: since sidecars became init containers with `restartPolicy: Always`
/// (K8s 1.28), `kubectl logs pod -c <sidecar>` names something that is not in
/// `spec.containers` at all, and a failed init container is the case where its
/// log is the only thing worth reading. Refusing those names answered the two
/// commonest debugging requests with "not valid for pod".
///
/// The *default* when no name is given stays `spec.containers` alone — an init
/// container that has already finished is not what `kubectl logs pod` means.
pub(crate) fn pick_container(
    pod: &Value,
    asked: Option<&str>,
    pod_name: &str,
) -> Result<String, ApiError> {
    let names = |field: &str| -> Vec<String> {
        pod["spec"][field]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|c| c["name"].as_str().map(str::to_string))
            .collect()
    };
    let containers = names("containers");
    match asked.filter(|c| !c.is_empty()) {
        Some(c) => {
            let mut all = containers.clone();
            all.extend(names("initContainers"));
            all.extend(names("ephemeralContainers"));
            if !all.iter().any(|x| x == c) {
                return Err(ApiError {
                    status: StatusCode::BAD_REQUEST,
                    reason: "BadRequest".into(),
                    message: format!(
                        "container {c} is not valid for pod {pod_name}; choose one of [{}]",
                        all.join(", ")
                    ),
                });
            }
            Ok(c.to_string())
        }
        None if containers.len() == 1 => Ok(containers[0].clone()),
        None => Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            reason: "BadRequest".into(),
            message: format!(
                "a container name must be specified for pod {pod_name}, choose one of: [{}]",
                containers.join(", ")
            ),
        }),
    }
}

/// A node's address, preferring InternalIP as upstream does.
pub(crate) async fn node_address(
    storage: &ResourceStorage,
    node_name: &str,
) -> Option<String> {
    let node: Value = storage
        .get(&ResourceStorage::cluster_key("nodes", node_name))
        .await
        .ok()?;
    let addrs = node["status"]["addresses"].as_array()?.clone();
    for want in ["InternalIP", "ExternalIP", "Hostname"] {
        if let Some(a) = addrs
            .iter()
            .find(|a| a["type"].as_str() == Some(want))
            .and_then(|a| a["address"].as_str())
        {
            return Some(a.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_init_or_ephemeral_container_is_a_valid_name() {
        // A sidecar is an init container with restartPolicy: Always (K8s 1.28),
        // and a failed init container's log is the one worth reading — both
        // used to come back "not valid for pod".
        let pod = json!({"spec": {
            "containers": [{"name": "app"}],
            "initContainers": [{"name": "setup"}, {"name": "sidecar"}],
            "ephemeralContainers": [{"name": "debugger"}]}});
        assert_eq!(pick_container(&pod, Some("setup"), "p").unwrap(), "setup");
        assert_eq!(pick_container(&pod, Some("sidecar"), "p").unwrap(), "sidecar");
        assert_eq!(pick_container(&pod, Some("debugger"), "p").unwrap(), "debugger");
        // The default is still the one real container: an init container that
        // has already finished is not what `kubectl logs pod` means.
        assert_eq!(pick_container(&pod, None, "p").unwrap(), "app");
    }

    #[test]
    fn an_unknown_container_lists_every_choice() {
        let pod = json!({"spec": {
            "containers": [{"name": "app"}],
            "initContainers": [{"name": "setup"}]}});
        let err = pick_container(&pod, Some("nope"), "p").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        // The init container is in the list, so the reader can see the name
        // they should have used rather than concluding it does not exist.
        assert!(err.message.contains("app"), "{}", err.message);
        assert!(err.message.contains("setup"), "{}", err.message);
    }

    #[test]
    fn the_container_is_required_when_there_is_more_than_one() {
        let pod = json!({"spec": {"containers": [{"name": "a"}, {"name": "b"}]}});
        let err = pick_container(&pod, None, "p").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("a, b"), "{}", err.message);
    }

    #[test]
    fn internal_ip_is_preferred_over_hostname() {
        // Upstream reaches a kubelet on its InternalIP; a Hostname may not
        // resolve from the apiserver, and picking it would work in a lab and
        // fail wherever DNS is not shared.
        let node = json!({"status":{"addresses":[
            {"type":"Hostname","address":"n1"},
            {"type":"InternalIP","address":"10.0.0.5"}]}});
        let addrs = node["status"]["addresses"].as_array().unwrap();
        let pick = |want: &str| {
            addrs.iter().find(|a| a["type"] == want).and_then(|a| a["address"].as_str())
        };
        assert_eq!(pick("InternalIP"), Some("10.0.0.5"));
        assert_eq!(pick("Hostname"), Some("n1"));
    }
}
