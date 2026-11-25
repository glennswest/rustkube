//! The RoleBindings OpenShift's default-rolebindings controller keeps in
//! every namespace: `system:image-builders` for builds' `builder` (#374) and
//! `system:deployers` for a Custom rollout's `deployer` (#392). Every
//! namespace is covered at start, then each one that appears or changes,
//! however it was created (a POST, a ProjectRequest, a manifest). The
//! controller-manager creates no RBAC objects (#176), so this is the
//! apiserver's job. A binding that exists is left as it is, so an admin who
//! edits one keeps the edit, as upstream does.

use crate::storage::ResourceStorage;
use serde_json::Value;
use std::sync::Arc;

/// A binding's name and how to make it for a namespace.
type Binding = (&'static str, fn(&str) -> Value);

/// Each binding a namespace gets.
fn bindings() -> [Binding; 2] {
    [
        (crate::builds::IMAGE_BUILDERS_BINDING, crate::builds::image_builders_binding),
        (crate::deployment_configs::DEPLOYERS_BINDING, crate::deployment_configs::deployers_binding),
    ]
}

async fn ensure(storage: &ResourceStorage, ns: &Value) {
    if !ns["metadata"]["deletionTimestamp"].is_null() || ns["status"]["phase"] == "Terminating" {
        return;
    }
    let Some(name) = ns["metadata"]["name"].as_str() else { return };
    for (binding_name, make) in bindings() {
        let key = ResourceStorage::namespaced_key("rolebindings", name, binding_name);
        if storage.get(&key).await.is_ok() {
            continue;
        }
        let mut binding = make(name);
        crate::handlers::resource::ensure_metadata_pub(&mut binding, binding_name, Some(name));
        match storage.create(&key, binding).await {
            Ok(_) => tracing::info!("rolebindings {name}/{binding_name} created"),
            Err(e) if e.reason == "AlreadyExists" => {}
            Err(e) => tracing::warn!("rolebindings {name}/{binding_name}: {}", e.message),
        }
    }
}

/// Watch the namespaces and keep their default bindings.
pub fn spawn(storage: Arc<ResourceStorage>) {
    tokio::spawn(async move {
        let prefix = ResourceStorage::cluster_prefix("namespaces");
        loop {
            let (items, _, rev) = match storage.list(&prefix, 0, None).await {
                Ok(page) => page,
                Err(e) => {
                    tracing::warn!("default-rolebindings: namespace list: {}", e.message);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            for ns in &items {
                ensure(&storage, ns).await;
            }
            match storage.watch(&prefix, rev).await {
                Ok(mut rx) => {
                    while let Some(ev) = rx.recv().await {
                        let value = match ev {
                            apimachinery::watch::WatchEvent::Added { value, .. }
                            | apimachinery::watch::WatchEvent::Modified { value, .. } => value,
                            _ => continue,
                        };
                        if let Ok(ns) = serde_json::from_slice::<Value>(&value) {
                            ensure(&storage, &ns).await;
                        }
                    }
                }
                Err(e) => tracing::warn!("default-rolebindings: namespace watch: {}", e.message),
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
}
