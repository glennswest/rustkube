//! HTTP request handlers for K8s resources.
//!
//! Provides generic CRUD+Watch handlers that work with any resource type,
//! plus specific route builders for core/v1 and apps/v1 resources.

pub mod resource;

use crate::crd::CrdRegistry;
use crate::storage::ResourceStorage;
use std::sync::Arc;

/// Shared API server state available to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<ResourceStorage>,
    pub crd_registry: Arc<CrdRegistry>,
}
