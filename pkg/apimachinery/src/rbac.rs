//! RBAC request and decision types — unused.
//!
//! Nothing references these. The engine that evaluates Roles and bindings,
//! with its own request type, is `apiserver::rbac_engine`.

/// An RBAC request to evaluate.
#[derive(Debug, Clone)]
pub struct AuthorizationRequest {
    pub user: String,
    pub groups: Vec<String>,
    pub verb: String,
    pub resource: String,
    pub api_group: String,
    pub namespace: Option<String>,
    pub name: Option<String>,
}

/// The result of an RBAC evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationDecision {
    Allowed,
    Denied { reason: String },
}
