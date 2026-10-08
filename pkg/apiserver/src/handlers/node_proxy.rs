//! `nodes/{name}/proxy` (#108): a request under
//! `/api/v1/nodes/{name}/proxy/{path}` is passed to that node's kubelet at
//! `https://<node address>:10250/{path}` — what `oc adm node-logs` (the
//! kubelet's `/logs/…`) and `kubectl get --raw …/proxy/stats/summary` or
//! `…/proxy/metrics` use.
//!
//! As upstream: authorized as the `nodes/proxy` subresource (GET is `get`,
//! POST `create`, PUT `update`, PATCH `patch`, DELETE `delete`), the method,
//! query, body and content type passed on, the kubelet's status, content type
//! and body streamed back. The apiserver authenticates to the kubelet with
//! its own token, as for `pods/log` (the kubelet's TokenReview); the
//! caller's credentials are not forwarded. Connection upgrades are not
//! proxied here (exec/attach/port-forward have their own subresources).

use crate::error::ApiError;
use crate::handlers::AppState;
use axum::extract::{Extension, Path, RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};

/// The kubelet path for a proxy request: the rest after `proxy`, `/` when none.
pub fn kubelet_path(rest: Option<&str>) -> String {
    format!("/{}", rest.unwrap_or("").trim_start_matches('/'))
}

async fn proxy(
    state: AppState,
    keys: crate::auth::SigningKeys,
    node: String,
    rest: Option<String>,
    query: Option<String>,
    method: Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if state.storage.get(&crate::storage::ResourceStorage::cluster_key("nodes", &node)).await.is_err() {
        return ApiError::not_found("nodes", &node).into_response();
    }
    let Some(addr) = crate::handlers::logs::node_address(&state.storage, &node).await else {
        return ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            reason: "ServiceUnavailable".into(),
            message: format!("node {node} has no address to proxy to"),
            continue_token: None,
        }
        .into_response();
    };
    let q = query.filter(|q| !q.is_empty()).map(|q| format!("?{q}")).unwrap_or_default();
    let url = format!("https://{addr}:10250{}{q}", kubelet_path(rest.as_deref()));
    // Unverified, as the log proxy: the kubelet's certificate is self-signed
    // until certificates are issued (rustkube#20). No total timeout: a
    // followed log is open-ended.
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return ApiError::internal(&format!("cannot build kubelet client: {e}")).into_response(),
    };
    let bearer = keys.create_token("system:kube-apiserver", &["system:masters".to_string()]).unwrap_or_default();
    let method = reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut req = client.request(method, &url).bearer_auth(&bearer);
    for h in [axum::http::header::CONTENT_TYPE, axum::http::header::ACCEPT] {
        if let Some(v) = headers.get(&h).and_then(|v| v.to_str().ok()) {
            req = req.header(h.as_str(), v);
        }
    }
    if !body.is_empty() {
        req = req.body(body);
    }
    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let ctype = resp.headers().get(reqwest::header::CONTENT_TYPE).cloned();
            let mut out = axum::body::Body::from_stream(resp.bytes_stream()).into_response();
            *out.status_mut() = status;
            if let Some(ct) = ctype.and_then(|c| axum::http::HeaderValue::from_bytes(c.as_bytes()).ok()) {
                out.headers_mut().insert(axum::http::header::CONTENT_TYPE, ct);
            }
            out
        }
        Err(e) => ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            reason: "ServiceUnavailable".into(),
            message: format!("reaching kubelet at {addr}: {e}"),
            continue_token: None,
        }
        .into_response(),
    }
}

/// `/api/v1/nodes/{name}/proxy`, every method.
pub async fn node_proxy_root(
    State(state): State<AppState>,
    Extension(keys): Extension<crate::auth::SigningKeys>,
    Path(node): Path<String>,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    proxy(state, keys, node, None, query, method, headers, body).await
}

/// `/api/v1/nodes/{name}/proxy/{*path}`, every method.
pub async fn node_proxy(
    State(state): State<AppState>,
    Extension(keys): Extension<crate::auth::SigningKeys>,
    Path((node, rest)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    proxy(state, keys, node, Some(rest), query, method, headers, body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rest_of_the_path_is_the_kubelet_path() {
        assert_eq!(kubelet_path(Some("logs/journal")), "/logs/journal");
        assert_eq!(kubelet_path(Some("stats/summary")), "/stats/summary");
        assert_eq!(kubelet_path(None), "/");
        assert_eq!(kubelet_path(Some("")), "/");
    }
}
