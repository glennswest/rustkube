//! Server-side field validation for built-in objects (#122): what
//! `?fieldValidation=` asks of a create or an update, as upstream answers it.
//!
//! - `Strict`: a body with a field its type does not have, or a key given
//!   twice, is refused — 400, `strict decoding error: unknown field
//!   "spec.foo", duplicate field "spec.replicas"`, upstream's wording.
//! - `Warn` (the default): accepted, each one named in a `Warning` header.
//! - `Ignore`: nothing said.
//!
//! The type is the endpoint's kind, its fields the vendored release-1.36
//! descriptors' ([`apimachinery::protobuf::unknown_fields`]); duplicates come
//! from the raw body. It covers a POST of a collection and a PUT of an object
//! or its `/status`, for the kinds that have a descriptor. Custom resources
//! are their schema's (`schema.rs`, #121); CustomResourceDefinitions are not
//! checked. Unlike upstream, which decodes into its types, a field accepted
//! under `Warn` or `Ignore` is stored as sent, not dropped.

use crate::schema::FieldValidation;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

const MAX_BODY: usize = 32 << 20;

/// The kind a write to `path` creates or replaces, for a collection POST or
/// an object (or `/status`) PUT of a built-in group; None for anything else.
pub(crate) fn written_kind(method: &Method, path: &str) -> Option<(String, String)> {
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    let rest = match segs.as_slice() {
        ["api", _v, rest @ ..] => rest,
        ["apis", _g, _v, rest @ ..] => rest,
        _ => return None,
    };
    let rest = match rest {
        ["namespaces", _ns, tail @ ..] if !tail.is_empty() => tail,
        other => other,
    };
    let ok = match (method, rest) {
        (&Method::POST, [_res]) => true,
        (&Method::PUT, [_res, _name]) | (&Method::PUT, [_res, _name, "status"]) => true,
        _ => false,
    };
    if !ok {
        return None;
    }
    let (av, kind) = crate::protobuf_mw::path_gvk(path);
    (!kind.is_empty()).then_some((av, kind))
}

/// The problems a body has, in upstream's words: unknown fields, then
/// duplicates. None when the kind is not checked here.
pub(crate) fn problems(body: &[u8], api_version: &str, kind: &str) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let unknown = apimachinery::protobuf::unknown_fields(&value, api_version, kind)?;
    let mut out: Vec<String> = unknown.into_iter().map(|u| format!("unknown field \"{u}\"")).collect();
    out.extend(
        crate::schema::json_duplicates(body).unwrap_or_default().into_iter().map(|d| format!("duplicate field \"{d}\"")),
    );
    Some(out)
}

pub async fn check(req: Request, next: Next) -> Response {
    let is_json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));
    let Some((av, kind)) = written_kind(req.method(), req.uri().path()).filter(|_| is_json) else {
        return next.run(req).await;
    };
    let validation = FieldValidation::from_query(req.uri().query().unwrap_or(""));
    if validation == FieldValidation::Ignore {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return crate::error::ApiError::bad_request("failed to read request body").into_response(),
    };
    let found = problems(&bytes, &av, &kind).unwrap_or_default();
    let req = Request::from_parts(parts, Body::from(bytes));
    if found.is_empty() {
        return next.run(req).await;
    }
    if validation == FieldValidation::Strict {
        return crate::error::ApiError::bad_request(&format!("strict decoding error: {}", found.join(", "))).into_response();
    }
    let mut resp = next.run(req).await;
    for f in &found {
        if let Ok(v) = HeaderValue::from_str(&crate::admission::warning_header(f)) {
            resp.headers_mut().append(header::WARNING, v);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_conformance_body_reads_as_upstream_words_it() {
        let body = br#"{"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "my-dep"},
            "spec": {"unknownField": "foo", "replicas": 2, "replicas": 3,
                     "selector": {"matchLabels": {"app": "nginx"}},
                     "template": {"metadata": {"labels": {"app": "nginx"}},
                                  "spec": {"containers": [{"name": "nginx", "image": "nginx:latest"}]}}}}"#;
        let p = problems(body, "apps/v1", "Deployment").unwrap();
        assert_eq!(format!("strict decoding error: {}", p.join(", ")),
                   r#"strict decoding error: unknown field "spec.unknownField", duplicate field "spec.replicas""#);
        assert!(problems(br#"{"metadata": {"name": "x"}}"#, "example.com/v1", "Thing").is_none(), "custom kinds are not checked here");
    }

    #[test]
    fn only_creates_and_replacements_of_built_ins_are_checked() {
        let k = |m: Method, p: &str| written_kind(&m, p);
        assert_eq!(k(Method::POST, "/apis/apps/v1/namespaces/d/deployments"), Some(("apps/v1".into(), "Deployment".into())));
        assert_eq!(k(Method::PUT, "/api/v1/namespaces/d/pods/p"), Some(("v1".into(), "Pod".into())));
        assert_eq!(k(Method::PUT, "/api/v1/nodes/n/status"), Some(("v1".into(), "Node".into())));
        assert!(k(Method::POST, "/api/v1/namespaces/d/pods/p/eviction").is_none());
        assert!(k(Method::POST, "/api/v1/namespaces/d/serviceaccounts/s/token").is_none());
        assert!(k(Method::PATCH, "/api/v1/namespaces/d/pods/p").is_none());
        assert!(k(Method::PUT, "/apis/apps/v1/namespaces/d/deployments/w/scale").is_none());
    }
}
