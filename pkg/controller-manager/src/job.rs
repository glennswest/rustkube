//! Job controller.
//!
//! Manages pods to completion for batch Jobs. Tracks active, succeeded,
//! and failed pod counts. Supports parallelism, completions, backoff limits,
//! and active deadline seconds.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

pub struct JobController {
    api: Arc<ApiClient>,
}

impl JobController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("Job controller started");
        let mut interval = time::interval(Duration::from_secs(2));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("Job reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(ns_name).await {
                debug!("Job reconcile in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let job_list: Value = self
            .api
            .list(&format!("/apis/batch/v1/namespaces/{namespace}/jobs"))
            .await?;
        let jobs = job_list["items"].as_array().cloned().unwrap_or_default();

        let pod_list: Value = self
            .api
            .list(&format!("/api/v1/namespaces/{namespace}/pods"))
            .await?;
        let pods = pod_list["items"].as_array().cloned().unwrap_or_default();

        for job in &jobs {
            if let Err(e) = self.reconcile_job(namespace, job, &pods).await {
                let name = job["metadata"]["name"].as_str().unwrap_or("?");
                warn!("Failed to reconcile job {namespace}/{name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_job(
        &self,
        namespace: &str,
        job: &Value,
        all_pods: &[Value],
    ) -> anyhow::Result<()> {
        let job_name = job["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("job missing name"))?;
        let job_uid = job["metadata"]["uid"].as_str().unwrap_or("");

        // Skip completed/failed jobs
        if let Some(conditions) = job["status"]["conditions"].as_array() {
            for cond in conditions {
                let ctype = cond["type"].as_str().unwrap_or("");
                let status = cond["status"].as_str().unwrap_or("");
                if (ctype == "Complete" || ctype == "Failed") && status == "True" {
                    return Ok(());
                }
            }
        }

        let completions = job["spec"]["completions"].as_u64().unwrap_or(1) as usize;
        let parallelism = job["spec"]["parallelism"].as_u64().unwrap_or(1) as usize;
        let backoff_limit = job["spec"]["backoffLimit"].as_u64().unwrap_or(6) as usize;
        let active_deadline = job["spec"]["activeDeadlineSeconds"].as_u64();

        // Find pods owned by this Job
        let owned_pods: Vec<&Value> = all_pods
            .iter()
            .filter(|pod| {
                pod["metadata"]["ownerReferences"]
                    .as_array()
                    .map(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(job_uid)))
                    .unwrap_or(false)
            })
            .collect();

        // Count pod states
        let mut active = 0usize;
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        for pod in &owned_pods {
            match pod["status"]["phase"].as_str().unwrap_or("Pending") {
                "Succeeded" => succeeded += 1,
                "Failed" => failed += 1,
                _ => active += 1, // Running or Pending
            }
        }

        // Check active deadline
        if let Some(deadline) = active_deadline {
            if let Some(start_time) = job["status"]["startTime"].as_str() {
                if let Ok(start) = chrono::DateTime::parse_from_rfc3339(
                    &start_time.replace('Z', "+00:00"),
                ) {
                    let elapsed = chrono::Utc::now()
                        .signed_duration_since(start.with_timezone(&chrono::Utc));
                    if elapsed.num_seconds() as u64 > deadline {
                        // Kill active pods and mark failed
                        for pod in &owned_pods {
                            let phase = pod["status"]["phase"].as_str().unwrap_or("");
                            if phase != "Succeeded" && phase != "Failed" {
                                let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
                                if !pod_name.is_empty() {
                                    let _ = self
                                        .api
                                        .delete(&format!(
                                            "/api/v1/namespaces/{namespace}/pods/{pod_name}"
                                        ))
                                        .await;
                                }
                            }
                        }
                        self.update_job_status(
                            namespace,
                            job_name,
                            job,
                            0,
                            succeeded,
                            failed,
                            Some(("Failed", "DeadlineExceeded")),
                        )
                        .await;
                        return Ok(());
                    }
                }
            }
        }

        // Check backoff limit
        if failed > backoff_limit {
            self.update_job_status(
                namespace,
                job_name,
                job,
                active,
                succeeded,
                failed,
                Some(("Failed", "BackoffLimitExceeded")),
            )
            .await;
            return Ok(());
        }

        // Check completion
        if succeeded >= completions {
            self.update_job_status(
                namespace,
                job_name,
                job,
                active,
                succeeded,
                failed,
                Some(("Complete", "Completed")),
            )
            .await;
            info!("Job {namespace}/{job_name} completed ({succeeded}/{completions} succeeded)");
            return Ok(());
        }

        // Create pods if needed
        if active < parallelism && succeeded + active < completions {
            let to_create =
                std::cmp::min(parallelism - active, completions - succeeded - active);
            for _ in 0..to_create {
                let pod = build_job_pod(namespace, job_name, job_uid, job)?;
                match self
                    .api
                    .create(&format!("/api/v1/namespaces/{namespace}/pods"), &pod)
                    .await
                {
                    Ok(_) => {
                        let pod_name = pod["metadata"]["name"].as_str().unwrap_or("?");
                        info!("Created pod {namespace}/{pod_name} for Job {job_name}");
                    }
                    Err(e) => {
                        warn!("Failed to create pod for Job {job_name}: {e}");
                    }
                }
            }
        }

        // Update status (no completion condition)
        self.update_job_status(namespace, job_name, job, active, succeeded, failed, None)
            .await;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_job_status(
        &self,
        namespace: &str,
        job_name: &str,
        job: &Value,
        active: usize,
        succeeded: usize,
        failed: usize,
        condition: Option<(&str, &str)>,
    ) {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        let mut updated = job.clone();
        let status = updated["status"].as_object_mut().map(|s| {
            s.insert("active".into(), json!(active));
            s.insert("succeeded".into(), json!(succeeded));
            s.insert("failed".into(), json!(failed));
            if !s.contains_key("startTime") {
                s.insert("startTime".into(), json!(now.clone()));
            }
        });
        if status.is_none() {
            updated["status"] = json!({
                "active": active,
                "succeeded": succeeded,
                "failed": failed,
                "startTime": now.clone()
            });
        }

        if let Some((ctype, reason)) = condition {
            let cond = json!({
                "type": ctype,
                "status": "True",
                "reason": reason,
                "lastTransitionTime": now.clone()
            });
            if ctype == "Complete" {
                updated["status"]["completionTime"] = json!(now);
            }
            let conditions = updated["status"]
                .as_object_mut()
                .unwrap()
                .entry("conditions")
                .or_insert_with(|| json!([]));
            if let Some(arr) = conditions.as_array_mut() {
                arr.push(cond);
            }
        }

        let _ = self
            .api
            .update(
                &format!("/apis/batch/v1/namespaces/{namespace}/jobs/{job_name}"),
                &updated,
            )
            .await;
    }
}

fn build_job_pod(
    namespace: &str,
    job_name: &str,
    job_uid: &str,
    job: &Value,
) -> anyhow::Result<Value> {
    let template = &job["spec"]["template"];
    let suffix = &uuid::Uuid::new_v4().to_string()[..5];
    let pod_name = format!("{job_name}-{suffix}");

    let mut labels = template["metadata"]["labels"].clone();
    if labels.is_null() {
        labels = json!({});
    }
    if let Some(map) = labels.as_object_mut() {
        map.insert("job-name".into(), Value::String(job_name.to_string()));
    }

    let mut spec = template["spec"].clone();
    if let Some(s) = spec.as_object_mut() {
        // Ensure restartPolicy is Never or OnFailure
        if !s.contains_key("restartPolicy") {
            s.insert("restartPolicy".into(), json!("Never"));
        }
    }

    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": pod_name,
            "namespace": namespace,
            "labels": labels,
            "ownerReferences": [{
                "apiVersion": "batch/v1",
                "kind": "Job",
                "name": job_name,
                "uid": job_uid,
                "controller": true,
                "blockOwnerDeletion": true
            }]
        },
        "spec": spec,
        "status": {
            "phase": "Pending"
        }
    });

    Ok(pod)
}
