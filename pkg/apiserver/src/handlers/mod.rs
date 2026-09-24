//! HTTP request handlers for K8s resources.
//!
//! Generic CRUD+watch handlers that work with any resource type
//! (`resource.rs`), plus logs, exec/attach/portforward, token,
//! `authorization.k8s.io` and kubevirt subresources. Routes are assembled in
//! `server.rs`.

pub mod authorization;
pub mod kubevirt;
pub mod logs;
pub mod resource;
pub mod streaming;
pub mod token;

use crate::crd::CrdRegistry;
use crate::storage::ResourceStorage;
use std::sync::Arc;

/// Shared API server state available to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<ResourceStorage>,
    pub crd_registry: Arc<CrdRegistry>,
    /// The range ClusterIPs are allocated from.
    pub service_cidr: String,
}
