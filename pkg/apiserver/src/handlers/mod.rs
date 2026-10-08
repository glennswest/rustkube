//! HTTP request handlers for K8s resources.
//!
//! Generic CRUD+watch handlers that work with any resource type
//! (`resource.rs`), plus logs, exec/attach/portforward, token,
//! `authorization.k8s.io`, kubevirt subresources and
//! `project.openshift.io` Projects over Namespaces. Routes are assembled in
//! `server.rs`.

pub mod authorization;
pub mod openshift_authorization;
pub mod hpa_v1;
pub mod kubevirt;
pub mod logs;
pub mod pod_resize;
pub mod project;
pub mod resource;
pub mod scale;
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
    /// Admission webhook configurations and their clients (#82).
    pub admission: Arc<crate::admission::Webhooks>,
    /// The APIServices this apiserver proxies to (#83).
    pub aggregator: Arc<crate::aggregation::Aggregator>,
    /// Where each node's cadvisor is, for `metrics.k8s.io` (#89).
    pub resource_metrics: Arc<crate::resource_metrics::ResourceMetrics>,
}
