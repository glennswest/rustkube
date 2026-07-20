//! Content negotiation for `application/vnd.kubernetes.protobuf`.
//!
//! This is the apiserver's only protobuf-specific code — the analog of
//! k8s.io/apiserver's negotiation glue. The codec itself lives in
//! `apimachinery::protobuf`. The middleware:
//!
//!   * request  — if `Content-Type` is protobuf, decode the body to JSON and
//!     hand a JSON request downstream, so every handler's `Json` extractor works
//!     unchanged. An undecodable/unsupported body gets a 415 Status.
//!   * response — if the client's `Accept` includes protobuf and the handler
//!     returned a JSON object with a known GVK, re-encode it to protobuf.
//!
//! Watch streams (`application/json;stream=watch`) are left as JSON: client-go's
//! `Accept` always includes JSON, and protobuf *frame* streaming is a separate
//! protocol we don't emit.

use apimachinery::protobuf;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{self, header, StatusCode};
use axum::response::Response;
use axum::middleware::Next;
use serde_json::{json, Value};

/// Max body we will buffer to transcode (matches the apiserver object ceiling
/// with headroom for large lists).
const MAX_BODY: usize = 16 * 1024 * 1024;

fn header_str<'a>(headers: &'a header::HeaderMap, name: header::HeaderName) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// Derive `(apiVersion, kind)` from a request path, so a protobuf body with an
/// empty envelope TypeMeta can still be decoded (#34). Returns empty strings for
/// paths that don't name a resource.
///
///   /api/v1/.../{resource}[/{name}[/{sub}]]            -> ("v1", Kind(resource))
///   /apis/{group}/{version}/.../{resource}[/{name}...] -> ("group/version", Kind)
fn path_gvk(path: &str) -> (String, String) {
    let segs: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let (api_version, rest): (String, &[&str]) = match segs.as_slice() {
        ["api", ver, tail @ ..] => (ver.to_string(), tail),
        ["apis", group, ver, tail @ ..] => (format!("{group}/{ver}"), tail),
        _ => return (String::new(), String::new()),
    };
    // The resource plural is the segment after an optional `namespaces/{ns}`
    // prefix; a trailing `/{name}` or `/{name}/{subresource}` doesn't change it.
    let resource = match rest {
        ["namespaces", _ns, res, ..] => Some(*res),
        [res, ..] if *res != "namespaces" => Some(*res),
        _ => None,
    };
    match resource {
        Some(r) => (api_version, crate::handlers::resource::resource_to_kind(r)),
        None => (api_version, String::new()),
    }
}

pub async fn transcode(req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();

    // GET/HEAD carry no body; every other verb may carry a protobuf body — a
    // resource for POST/PUT/PATCH, a meta/v1 DeleteOptions for DELETE.
    let has_body = !matches!(parts.method, http::Method::GET | http::Method::HEAD);
    let req_is_pb =
        has_body && protobuf::wants_protobuf(header_str(&parts.headers, header::CONTENT_TYPE));
    let accept_pb = header_str(&parts.headers, header::ACCEPT).contains(protobuf::CONTENT_TYPE);
    // GVK implied by the endpoint, used when the client's protobuf envelope has
    // no TypeMeta (typed client-go clients often leave it blank — #34). A DELETE
    // body is always a meta/v1 DeleteOptions regardless of the endpoint.
    let (hint_av, hint_kind) = if parts.method == http::Method::DELETE {
        ("v1".to_string(), "DeleteOptions".to_string())
    } else {
        path_gvk(parts.uri.path())
    };

    // --- request: protobuf body -> JSON body -------------------------------
    let req = if req_is_pb {
        let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
            Ok(b) => b,
            Err(_) => return status_response(StatusCode::BAD_REQUEST, "failed to read request body"),
        };
        if bytes.is_empty() {
            // No body to transcode (e.g. DELETE with no DeleteOptions). Hand an
            // empty JSON body downstream (still transcode the response below).
            parts.headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            parts.headers.remove(header::CONTENT_LENGTH);
            Request::from_parts(parts, Body::empty())
        } else {
            match protobuf::decode_to_json(&bytes, &hint_av, &hint_kind) {
                Ok(value) => {
                    let json = serde_json::to_vec(&value).unwrap_or_default();
                    parts.headers.insert(
                        header::CONTENT_TYPE,
                        header::HeaderValue::from_static("application/json"),
                    );
                    parts.headers.remove(header::CONTENT_LENGTH);
                    Request::from_parts(parts, Body::from(json))
                }
                Err(e) => {
                    // Well-behaved clients that get a 415 with a Status can retry
                    // as JSON; ill-formed protobuf is a client error either way.
                    return status_response(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        &format!("cannot decode protobuf request: {e}"),
                    );
                }
            }
        }
    } else {
        Request::from_parts(parts, body)
    };

    let resp = next.run(req).await;

    // --- response: JSON body -> protobuf body (only if the client asked) ----
    if !accept_pb {
        return resp;
    }
    let (mut rparts, rbody) = resp.into_parts();
    let rct = header_str(&rparts.headers, header::CONTENT_TYPE);
    // Only transcode plain JSON object bodies — never watch streams.
    if !rct.starts_with("application/json") || rct.contains("stream=watch") {
        return Response::from_parts(rparts, rbody);
    }

    let bytes = match axum::body::to_bytes(rbody, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return status_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to read response body"),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Response::from_parts(rparts, Body::from(bytes)),
    };

    let api_version = value.get("apiVersion").and_then(Value::as_str).unwrap_or("");
    let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
    if protobuf::supports(api_version, kind) {
        if let Ok(pb) = protobuf::encode_from_json(&value, api_version, kind) {
            rparts.headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static(protobuf::CONTENT_TYPE),
            );
            rparts.headers.remove(header::CONTENT_LENGTH);
            return Response::from_parts(rparts, Body::from(pb));
        }
    }
    // No protobuf schema for this GVK (or a Status/error object): the client's
    // Accept includes JSON, so returning JSON is compliant.
    Response::from_parts(rparts, Body::from(bytes))
}

fn status_response(code: StatusCode, message: &str) -> Response {
    let body = json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": message,
        "code": code.as_u16(),
    });
    let bytes = Bytes::from(serde_json::to_vec(&body).unwrap_or_default());
    Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::path_gvk;

    #[test]
    fn path_gvk_derivation() {
        // Core, namespaced, subresource, and grouped/CRD paths.
        assert_eq!(path_gvk("/api/v1/namespaces/default/configmaps"), ("v1".into(), "ConfigMap".into()));
        assert_eq!(
            path_gvk("/apis/apiextensions.k8s.io/v1/customresourcedefinitions"),
            ("apiextensions.k8s.io/v1".into(), "CustomResourceDefinition".into())
        );
        assert_eq!(
            path_gvk("/api/v1/namespaces/default/pods/p1"),
            ("v1".into(), "Pod".into())
        );
        assert_eq!(
            path_gvk("/apis/apps/v1/namespaces/default/deployments/d/status"),
            ("apps/v1".into(), "Deployment".into())
        );
        assert_eq!(
            path_gvk("/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/lock"),
            ("coordination.k8s.io/v1".into(), "Lease".into())
        );
        // Non-resource paths yield no GVK.
        assert_eq!(path_gvk("/healthz"), ("".into(), "".into()));
    }
}
