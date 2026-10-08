//! 404 for a resource a built-in group-version does not serve (#110).
//!
//! The generic handlers sit behind catch-all routes
//! (`/api/v1/{resource}`, `/apis/apps/v1/namespaces/{namespace}/{resource}`,
//! …), so any plural reached them: `GET /api/v1/replicationcontrollers`,
//! before it was served, answered 200 with an empty list of kind
//! `replicationcontrollersList` (client-go: "no kind … is registered"), a
//! typo answered an empty list, and a write to it was stored. Upstream
//! answers 404 `the server could not find the requested resource`.
//!
//! This middleware answers that, for every method, when the path's resource
//! is not in its group-version's discovery list ([`crate::discovery::advertised`],
//! built from the discovery handlers themselves, so what is checked is what
//! clients are told). It covers only group-versions the apiserver serves
//! itself; custom resources, aggregated APIs and the groups served by their
//! own routes (events, metrics) are left to their handlers. A resource of a
//! registered CRD in a built-in group (Gateway API CRDs beside the built-in
//! Gateway kinds) still passes.

use crate::handlers::AppState;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// `(group, version, resource)` of a resource path, or None.
pub fn resource_of(path: &str) -> Option<(String, String, String)> {
    let segs: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let (group, version, rest): (&str, &str, &[&str]) = match segs.as_slice() {
        ["api", v, rest @ ..] => ("", v, rest),
        ["apis", g, v, rest @ ..] => (g, v, rest),
        _ => return None,
    };
    let resource = match rest {
        [] => return None,
        // `namespaces` itself, one namespace, or one of its subresources.
        ["namespaces"] | ["namespaces", _] | ["namespaces", _, "status" | "finalize"] => "namespaces",
        ["namespaces", _, res, ..] => res,
        [res, ..] => res,
    };
    Some((group.to_string(), version.to_string(), resource.to_string()))
}

pub async fn check(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if let Some((group, version, resource)) = resource_of(req.uri().path()) {
        if let Some(served) = crate::discovery::advertised(&group, &version).await {
            if !served.contains(&resource) && state.crd_registry.lookup(&group, &version, &resource).await.is_none() {
                return crate::error::ApiError {
                    status: axum::http::StatusCode::NOT_FOUND,
                    reason: "NotFound".into(),
                    message: "the server could not find the requested resource".into(),
                    continue_token: None,
                }
                .into_response();
            }
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resource_is_read_from_every_path_shape() {
        let r = |p: &str| resource_of(p).map(|(g, v, r)| format!("{g}|{v}|{r}"));
        assert_eq!(r("/api/v1/replicationcontrollers").as_deref(), Some("|v1|replicationcontrollers"));
        assert_eq!(r("/api/v1/namespaces").as_deref(), Some("|v1|namespaces"));
        assert_eq!(r("/api/v1/namespaces/default").as_deref(), Some("|v1|namespaces"));
        assert_eq!(r("/api/v1/namespaces/default/finalize").as_deref(), Some("|v1|namespaces"));
        assert_eq!(r("/api/v1/namespaces/default/pods/p/log").as_deref(), Some("|v1|pods"));
        assert_eq!(r("/apis/apps/v1/namespaces/d/deployments/web/scale").as_deref(), Some("apps|v1|deployments"));
        assert_eq!(r("/apis/rbac.authorization.k8s.io/v1/clusterroles").as_deref(), Some("rbac.authorization.k8s.io|v1|clusterroles"));
        assert_eq!(r("/api/v1"), None);
        assert_eq!(r("/apis/apps"), None);
        assert_eq!(r("/healthz"), None);
    }
}
