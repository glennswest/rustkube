//! rk-apiserver: Kubernetes-compatible REST API server.
//!
//! Serves the full K8s REST API via axum, with authentication (x509, JWT, OIDC),
//! RBAC authorization, admission control, watch cache, and API group registry.
//! Wire-compatible with kubectl, helm, and existing K8s client libraries.
