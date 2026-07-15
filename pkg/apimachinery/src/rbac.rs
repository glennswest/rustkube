//! RBAC evaluation engine.
//!
//! Evaluates whether a given user (identified by username and groups)
//! is authorized to perform a specific verb on a resource, based on
//! the set of Role/ClusterRole bindings in the cluster.

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
