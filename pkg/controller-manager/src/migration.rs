//! Pod migration controller.
//!
//! Watches PodMigration resources and drives the migration state machine:
//!   Pending → Checkpointing → Transferring → Restoring → Verifying → Completed
//!
//! Communication with kubelets uses pod annotations:
//! - `rustkube.io/migrate-action` — tells kubelet what to do
//! - `rustkube.io/checkpoint-ref` — checkpoint artifact reference
//! - `rustkube.io/migration-endpoint` — live migration target endpoint
//! - `rustkube.io/restore-from` — checkpoint to restore from
//!
//! Supports per-runtime strategies:
//! - Checkpoint (CRIU): native containers (~100ms downtime)
//! - LiveMigrate: QEMU/cloud-hypervisor VMs (~10-50ms downtime)
//! - Snapshot: Firecracker VMs (~200ms downtime)
//! - Evacuate: CRI pods (kill + reschedule, seconds of downtime)

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, info, warn};

pub struct MigrationController {
    api: Arc<ApiClient>,
}

impl MigrationController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("Migration controller started");
        let mut interval = time::interval(Duration::from_secs(2));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                debug!("Migration reconcile: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        // List all namespaces
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(ns_name).await {
                debug!("Migration reconcile in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let migration_list: Value = self
            .api
            .list(&format!(
                "/apis/rustkube.io/v1alpha1/namespaces/{namespace}/podmigrations"
            ))
            .await?;
        let migrations = migration_list["items"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        for migration in &migrations {
            let name = migration["metadata"]["name"].as_str().unwrap_or("?");
            let phase = migration["status"]["phase"]
                .as_str()
                .unwrap_or("Pending");

            // Skip terminal states
            if phase == "Completed" || phase == "Failed" {
                continue;
            }

            if let Err(e) = self.reconcile_migration(namespace, migration).await {
                warn!("Failed to reconcile migration {namespace}/{name}: {e}");
                // Update status to Failed
                let _ = self
                    .update_migration_status(
                        namespace,
                        name,
                        "Failed",
                        &e.to_string(),
                        None,
                    )
                    .await;
            }
        }
        Ok(())
    }

    async fn reconcile_migration(
        &self,
        namespace: &str,
        migration: &Value,
    ) -> anyhow::Result<()> {
        let name = migration["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("migration missing name"))?;
        let phase = migration["status"]["phase"]
            .as_str()
            .unwrap_or("Pending");
        let pod_name = migration["spec"]["podName"]
            .as_str()
            .unwrap_or("");
        let source_node = migration["spec"]["sourceNode"]
            .as_str()
            .unwrap_or("");
        let target_node = migration["spec"]["targetNode"]
            .as_str()
            .unwrap_or("");
        let strategy = migration["spec"]["strategy"]
            .as_str()
            .unwrap_or("auto");
        let timeout_secs = migration["spec"]["timeout"]
            .as_u64()
            .unwrap_or(300);
        let delete_source = migration["spec"]["deleteSourcePod"]
            .as_bool()
            .unwrap_or(true);

        // Check timeout
        if let Some(start_time) = migration["status"]["startTime"].as_str() {
            if let Ok(started) = chrono::DateTime::parse_from_rfc3339(start_time) {
                let elapsed = chrono::Utc::now()
                    .signed_duration_since(started)
                    .num_seconds();
                if elapsed > timeout_secs as i64 {
                    return Err(anyhow::anyhow!(
                        "migration timed out after {elapsed}s (limit {timeout_secs}s)"
                    ));
                }
            }
        }

        match phase {
            "Pending" => {
                self.phase_pending(namespace, name, pod_name, source_node, target_node, strategy)
                    .await
            }
            "Checkpointing" => {
                self.phase_checkpointing(namespace, name, pod_name).await
            }
            "Transferring" => {
                self.phase_transferring(namespace, name, pod_name, target_node, migration)
                    .await
            }
            "Restoring" => {
                self.phase_restoring(namespace, name, pod_name, target_node, migration)
                    .await
            }
            "Verifying" => {
                self.phase_verifying(namespace, name, pod_name, target_node, delete_source, source_node)
                    .await
            }
            _ => {
                debug!("Unknown migration phase: {phase}");
                Ok(())
            }
        }
    }

    /// Pending: validate pod exists on source, target is Ready, determine strategy.
    async fn phase_pending(
        &self,
        namespace: &str,
        name: &str,
        pod_name: &str,
        source_node: &str,
        target_node: &str,
        strategy: &str,
    ) -> anyhow::Result<()> {
        info!("Migration {namespace}/{name}: Pending → validating");

        // Validate source pod exists
        let pod_resp = self
            .api
            .get(&format!(
                "/api/v1/namespaces/{namespace}/pods/{pod_name}"
            ))
            .await?;

        if !pod_resp.status().is_success() {
            return Err(anyhow::anyhow!("pod {namespace}/{pod_name} not found"));
        }

        let pod: Value = pod_resp.json().await?;

        // Verify pod is on source node
        let actual_node = pod["spec"]["nodeName"].as_str().unwrap_or("");
        if actual_node != source_node {
            return Err(anyhow::anyhow!(
                "pod is on node '{actual_node}', expected '{source_node}'"
            ));
        }

        // Validate target node exists and is Ready
        let node_resp = self
            .api
            .get(&format!("/api/v1/nodes/{target_node}"))
            .await?;

        if !node_resp.status().is_success() {
            return Err(anyhow::anyhow!("target node '{target_node}' not found"));
        }

        let node: Value = node_resp.json().await?;
        let is_ready = node["status"]["conditions"]
            .as_array()
            .map(|conds| {
                conds.iter().any(|c| {
                    c["type"].as_str() == Some("Ready")
                        && c["status"].as_str() == Some("True")
                })
            })
            .unwrap_or(false);

        if !is_ready {
            return Err(anyhow::anyhow!("target node '{target_node}' is not Ready"));
        }

        // Determine effective strategy
        let effective_strategy = if strategy == "auto" {
            // Determine from runtime class annotation
            let runtime_class = pod["spec"]["runtimeClassName"]
                .as_str()
                .unwrap_or("");
            match runtime_class {
                "vm-cloud-hypervisor" | "vm-qemu" => "live",
                "vm-firecracker" => "snapshot",
                "native" => "checkpoint",
                _ => "evacuate",
            }
        } else {
            strategy
        };

        // Set start time and advance to Checkpointing
        let now = chrono::Utc::now().to_rfc3339();

        // Annotate source pod with migration action
        let action = match effective_strategy {
            "live" => "prepare-target",
            "checkpoint" | "snapshot" => "checkpoint",
            _ => "evacuate",
        };

        // Set annotation on source pod
        let _ = self
            .api
            .patch(
                &format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"),
                &json!({
                    "metadata": {
                        "annotations": {
                            "rustkube.io/migrate-action": action,
                            "rustkube.io/migrate-strategy": effective_strategy,
                            "rustkube.io/migrate-target-node": target_node,
                        }
                    }
                }),
            )
            .await;

        // For evacuate, skip directly to Restoring (just reschedule)
        let next_phase = if effective_strategy == "evacuate" {
            "Restoring"
        } else {
            "Checkpointing"
        };

        self.update_migration_status(
            namespace,
            name,
            next_phase,
            &format!("strategy={effective_strategy}, action={action}"),
            Some(&now),
        )
        .await?;

        info!("Migration {namespace}/{name}: → {next_phase} (strategy={effective_strategy})");
        Ok(())
    }

    /// Checkpointing: wait for source kubelet to write checkpoint ref.
    async fn phase_checkpointing(
        &self,
        namespace: &str,
        name: &str,
        pod_name: &str,
    ) -> anyhow::Result<()> {
        // Check if kubelet has written the checkpoint ref annotation
        let pod_resp = self
            .api
            .get(&format!(
                "/api/v1/namespaces/{namespace}/pods/{pod_name}"
            ))
            .await?;

        if !pod_resp.status().is_success() {
            return Ok(()); // Pod may not exist yet, retry
        }

        let pod: Value = pod_resp.json().await?;
        let checkpoint_ref = pod["metadata"]["annotations"]["rustkube.io/checkpoint-ref"]
            .as_str();
        let migration_endpoint = pod["metadata"]["annotations"]["rustkube.io/migration-endpoint"]
            .as_str();

        if checkpoint_ref.is_some() || migration_endpoint.is_some() {
            // Checkpoint or migration endpoint is ready — advance to Transferring
            let mut status_msg = String::new();
            if let Some(cr) = checkpoint_ref {
                status_msg = format!("checkpoint-ref={cr}");
            }
            if let Some(ep) = migration_endpoint {
                status_msg = format!("migration-endpoint={ep}");
            }

            self.update_migration_status(namespace, name, "Transferring", &status_msg, None)
                .await?;
            info!("Migration {namespace}/{name}: Checkpointing → Transferring");
        } else {
            debug!("Migration {namespace}/{name}: waiting for kubelet checkpoint");
        }

        Ok(())
    }

    /// Transferring: for checkpoint, transfer archive; for live, initiate migration.
    async fn phase_transferring(
        &self,
        namespace: &str,
        name: &str,
        pod_name: &str,
        target_node: &str,
        _migration: &Value,
    ) -> anyhow::Result<()> {
        let pod_resp = self
            .api
            .get(&format!(
                "/api/v1/namespaces/{namespace}/pods/{pod_name}"
            ))
            .await?;

        if !pod_resp.status().is_success() {
            return Ok(());
        }

        let pod: Value = pod_resp.json().await?;
        let strategy = pod["metadata"]["annotations"]["rustkube.io/migrate-strategy"]
            .as_str()
            .unwrap_or("evacuate");

        match strategy {
            "checkpoint" | "snapshot" => {
                // Set annotation on source pod telling kubelet to serve the checkpoint
                // and on target pod to download it
                let checkpoint_ref = pod["metadata"]["annotations"]["rustkube.io/checkpoint-ref"]
                    .as_str()
                    .unwrap_or("");

                // Set restore-from annotation so target kubelet picks it up
                let _ = self
                    .api
                    .patch(
                        &format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"),
                        &json!({
                            "metadata": {
                                "annotations": {
                                    "rustkube.io/migrate-action": "transfer-complete",
                                    "rustkube.io/restore-target-node": target_node,
                                }
                            }
                        }),
                    )
                    .await;

                self.update_migration_status(
                    namespace,
                    name,
                    "Restoring",
                    &format!("checkpoint={checkpoint_ref}, target={target_node}"),
                    None,
                )
                .await?;
                info!("Migration {namespace}/{name}: Transferring → Restoring");
            }
            "live" => {
                // For live migration, tell source kubelet to initiate the migration
                let endpoint = pod["metadata"]["annotations"]["rustkube.io/migration-endpoint"]
                    .as_str()
                    .unwrap_or("");

                let _ = self
                    .api
                    .patch(
                        &format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"),
                        &json!({
                            "metadata": {
                                "annotations": {
                                    "rustkube.io/migrate-action": "live-migrate",
                                    "rustkube.io/migration-target-endpoint": endpoint,
                                }
                            }
                        }),
                    )
                    .await;

                self.update_migration_status(
                    namespace,
                    name,
                    "Restoring",
                    &format!("live migration to {endpoint}"),
                    None,
                )
                .await?;
                info!("Migration {namespace}/{name}: Transferring → Restoring (live)");
            }
            _ => {
                // Evacuate — just advance
                self.update_migration_status(
                    namespace,
                    name,
                    "Restoring",
                    "evacuate: creating new pod on target",
                    None,
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Restoring: create new pod on target (for checkpoint/evacuate) or wait for live migration.
    async fn phase_restoring(
        &self,
        namespace: &str,
        name: &str,
        pod_name: &str,
        target_node: &str,
        migration: &Value,
    ) -> anyhow::Result<()> {
        let strategy = migration["status"]["message"]
            .as_str()
            .unwrap_or("");

        // For evacuate, create a new pod on the target node
        if strategy.contains("evacuate") || strategy.contains("strategy=evacuate") {
            // Get the source pod spec
            let pod_resp = self
                .api
                .get(&format!(
                    "/api/v1/namespaces/{namespace}/pods/{pod_name}"
                ))
                .await?;

            if pod_resp.status().is_success() {
                let mut pod: Value = pod_resp.json().await?;

                // Delete source pod first
                let _ = self
                    .api
                    .delete(&format!(
                        "/api/v1/namespaces/{namespace}/pods/{pod_name}"
                    ))
                    .await;

                // Create new pod on target node
                pod["spec"]["nodeName"] = json!(target_node);
                // Clear status and metadata that shouldn't carry over
                pod["status"] = json!({});
                pod["metadata"]["uid"] = json!(null);
                pod["metadata"]["resourceVersion"] = json!(null);
                pod["metadata"]["creationTimestamp"] = json!(null);
                if let Some(annotations) = pod["metadata"]["annotations"].as_object_mut() {
                    annotations.remove("rustkube.io/migrate-action");
                    annotations.remove("rustkube.io/migrate-strategy");
                    annotations.remove("rustkube.io/migrate-target-node");
                    annotations.remove("rustkube.io/checkpoint-ref");
                    annotations.remove("rustkube.io/migration-endpoint");
                }

                let _ = self
                    .api
                    .create(
                        &format!("/api/v1/namespaces/{namespace}/pods"),
                        &pod,
                    )
                    .await;
            }
        }

        self.update_migration_status(
            namespace,
            name,
            "Verifying",
            "waiting for new pod to become Ready",
            None,
        )
        .await?;
        info!("Migration {namespace}/{name}: Restoring → Verifying");

        Ok(())
    }

    /// Verifying: confirm new pod is Running+Ready, delete source if requested.
    async fn phase_verifying(
        &self,
        namespace: &str,
        name: &str,
        pod_name: &str,
        target_node: &str,
        delete_source: bool,
        source_node: &str,
    ) -> anyhow::Result<()> {
        // Check if new pod is running on target node
        let pod_resp = self
            .api
            .get(&format!(
                "/api/v1/namespaces/{namespace}/pods/{pod_name}"
            ))
            .await?;

        if !pod_resp.status().is_success() {
            debug!("Migration {namespace}/{name}: pod not found yet, waiting");
            return Ok(());
        }

        let pod: Value = pod_resp.json().await?;
        let phase = pod["status"]["phase"].as_str().unwrap_or("Pending");
        let node = pod["spec"]["nodeName"].as_str().unwrap_or("");

        if phase == "Running" && node == target_node {
            // Check if Ready
            let is_ready = pod["status"]["conditions"]
                .as_array()
                .map(|conds| {
                    conds.iter().any(|c| {
                        c["type"].as_str() == Some("Ready")
                            && c["status"].as_str() == Some("True")
                    })
                })
                .unwrap_or(false);

            if is_ready {
                let now = chrono::Utc::now().to_rfc3339();
                self.update_migration_status_full(
                    namespace,
                    name,
                    "Completed",
                    &format!("pod migrated to {target_node}"),
                    None,
                    Some(&now),
                )
                .await?;
                info!("Migration {namespace}/{name}: Verifying → Completed");
                return Ok(());
            }
        }

        // Pod not ready yet — if evacuate strategy and pod is still on source, it hasn't moved yet
        if phase == "Running" && node == source_node && delete_source {
            // The old pod is still running; the new one may not have been created yet.
            // Just wait — don't delete prematurely.
        }

        debug!("Migration {namespace}/{name}: pod phase={phase} node={node}, waiting for Ready on {target_node}");
        Ok(())
    }

    /// Initiate migration of all pods on a node (for node drain).
    pub async fn drain_node(&self, node_name: &str) -> anyhow::Result<Vec<String>> {
        info!("Draining node {node_name}");

        // Taint the node
        let _ = self
            .api
            .patch(
                &format!("/api/v1/nodes/{node_name}"),
                &json!({
                    "spec": {
                        "taints": [{
                            "key": "rustkube.io/draining",
                            "effect": "NoSchedule"
                        }]
                    }
                }),
            )
            .await;

        // List all pods on the node
        let pod_list: Value = self.api.list("/api/v1/pods").await?;
        let pods = pod_list["items"].as_array().cloned().unwrap_or_default();

        let mut migration_names = Vec::new();

        for pod in &pods {
            let pod_node = pod["spec"]["nodeName"].as_str().unwrap_or("");
            if pod_node != node_name {
                continue;
            }

            let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
            let namespace = pod["metadata"]["namespace"].as_str().unwrap_or("default");

            // Skip DaemonSet pods
            let is_daemonset = pod["metadata"]["ownerReferences"]
                .as_array()
                .map(|refs| refs.iter().any(|r| r["kind"].as_str() == Some("DaemonSet")))
                .unwrap_or(false);

            if is_daemonset {
                debug!("Skipping DaemonSet pod {namespace}/{pod_name}");
                continue;
            }

            // Create PodMigration resource
            let migration = json!({
                "apiVersion": "rustkube.io/v1alpha1",
                "kind": "PodMigration",
                "metadata": {
                    "name": format!("drain-{pod_name}"),
                    "namespace": namespace,
                },
                "spec": {
                    "podName": pod_name,
                    "sourceNode": node_name,
                    "targetNode": "",  // Let scheduler pick
                    "strategy": "auto",
                    "timeout": 300,
                    "deleteSourcePod": true,
                },
                "status": {
                    "phase": "Pending",
                    "message": format!("drain of node {node_name}"),
                }
            });

            match self
                .api
                .create(
                    &format!(
                        "/apis/rustkube.io/v1alpha1/namespaces/{namespace}/podmigrations"
                    ),
                    &migration,
                )
                .await
            {
                Ok(_) => {
                    let mig_name = format!("drain-{pod_name}");
                    info!("Created migration {namespace}/{mig_name} for drain");
                    migration_names.push(mig_name);
                }
                Err(e) => {
                    warn!("Failed to create migration for {namespace}/{pod_name}: {e}");
                }
            }
        }

        info!(
            "Node drain initiated: {} migrations created",
            migration_names.len()
        );
        Ok(migration_names)
    }

    async fn update_migration_status(
        &self,
        namespace: &str,
        name: &str,
        phase: &str,
        message: &str,
        start_time: Option<&str>,
    ) -> anyhow::Result<()> {
        self.update_migration_status_full(namespace, name, phase, message, start_time, None)
            .await
    }

    async fn update_migration_status_full(
        &self,
        namespace: &str,
        name: &str,
        phase: &str,
        message: &str,
        start_time: Option<&str>,
        completion_time: Option<&str>,
    ) -> anyhow::Result<()> {
        let path = format!(
            "/apis/rustkube.io/v1alpha1/namespaces/{namespace}/podmigrations/{name}"
        );

        // Get current resource
        let resp = self.api.get(&path).await?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("migration {namespace}/{name} not found"));
        }
        let mut migration: Value = resp.json().await?;

        migration["status"]["phase"] = json!(phase);
        migration["status"]["message"] = json!(message);
        if let Some(st) = start_time {
            migration["status"]["startTime"] = json!(st);
        }
        if let Some(ct) = completion_time {
            migration["status"]["completionTime"] = json!(ct);
        }

        let _ = self.api.update(&path, &migration).await;
        Ok(())
    }
}
