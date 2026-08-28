//! CronJob controller.
//!
//! Creates Jobs on a cron schedule. Supports standard 5-field cron expressions
//! (minute, hour, day-of-month, month, day-of-week). Handles concurrency
//! policies (Allow, Forbid, Replace) and history limits.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

pub struct CronJobController {
    api: Arc<ApiClient>,
    /// Every scheduling decision becomes an Event.
    ///
    /// This is the audit trail. A CronJob's own status says what is true now;
    /// it cannot say "at 02:00 the run was skipped because the previous one
    /// was still going", and that is exactly the question asked afterwards.
    /// Events are also what `oc describe cronjob` prints, so the history shows
    /// up with no client changes.
    recorder: crate::events::EventRecorder,
}

impl CronJobController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self {
            recorder: crate::events::EventRecorder::new(api.clone(), "cronjob-controller"),
            api,
        }
    }

    pub async fn run(&self) {
        info!("CronJob controller started");
        let mut interval = time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("CronJob reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(ns_name).await {
                debug!("CronJob reconcile in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let cj_list: Value = self
            .api
            .list(&format!(
                "/apis/batch/v1/namespaces/{namespace}/cronjobs"
            ))
            .await?;
        let cronjobs = cj_list["items"].as_array().cloned().unwrap_or_default();

        let job_list: Value = self
            .api
            .list(&format!("/apis/batch/v1/namespaces/{namespace}/jobs"))
            .await?;
        let jobs = job_list["items"].as_array().cloned().unwrap_or_default();

        for cj in &cronjobs {
            if let Err(e) = self.reconcile_cronjob(namespace, cj, &jobs).await {
                let name = cj["metadata"]["name"].as_str().unwrap_or("?");
                warn!("Failed to reconcile cronjob {namespace}/{name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_cronjob(
        &self,
        namespace: &str,
        cj: &Value,
        all_jobs: &[Value],
    ) -> anyhow::Result<()> {
        let cj_name = cj["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("cronjob missing name"))?;
        let cj_uid = cj["metadata"]["uid"].as_str().unwrap_or("");
        let schedule = cj["spec"]["schedule"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("cronjob missing schedule"))?;
        let concurrency = cj["spec"]["concurrencyPolicy"]
            .as_str()
            .unwrap_or("Allow");
        let suspend = cj["spec"]["suspend"].as_bool().unwrap_or(false);
        let successful_limit = cj["spec"]["successfulJobsHistoryLimit"]
            .as_u64()
            .unwrap_or(3) as usize;
        let failed_limit = cj["spec"]["failedJobsHistoryLimit"]
            .as_u64()
            .unwrap_or(1) as usize;

        if suspend {
            return Ok(());
        }

        // Find jobs owned by this CronJob
        let owned_jobs: Vec<&Value> = all_jobs
            .iter()
            .filter(|job| {
                job["metadata"]["ownerReferences"]
                    .as_array()
                    .map(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(cj_uid)))
                    .unwrap_or(false)
            })
            .collect();

        // Categorize jobs
        let mut active_jobs: Vec<&Value> = Vec::new();
        let mut successful_jobs: Vec<&Value> = Vec::new();
        let mut failed_jobs: Vec<&Value> = Vec::new();

        for job in &owned_jobs {
            let is_complete = job["status"]["conditions"]
                .as_array()
                .map(|conds| {
                    conds.iter().any(|c| {
                        c["type"].as_str() == Some("Complete")
                            && c["status"].as_str() == Some("True")
                    })
                })
                .unwrap_or(false);
            let is_failed = job["status"]["conditions"]
                .as_array()
                .map(|conds| {
                    conds.iter().any(|c| {
                        c["type"].as_str() == Some("Failed")
                            && c["status"].as_str() == Some("True")
                    })
                })
                .unwrap_or(false);

            if is_complete {
                successful_jobs.push(job);
            } else if is_failed {
                failed_jobs.push(job);
            } else {
                active_jobs.push(job);
            }
        }

        // What start, if any, is owed right now.
        //
        // Not "does the schedule match this instant": the controller ticks
        // every five seconds and a schedule fires on a minute, so asking only
        // about *now* loses every run the controller was not awake for — a
        // restart, a slow reconcile, a node that was paused. The window since
        // the last run is what gets evaluated, and only the most recent missed
        // start is acted on.
        let now = chrono::Utc::now();
        let created = cj["metadata"]["creationTimestamp"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or(now);
        let last_schedule = cj["status"]["lastScheduleTime"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc));
        let deadline = cj["spec"]["startingDeadlineSeconds"].as_i64();

        // A time zone is not honoured, and saying so beats scheduling in the
        // wrong one: `spec.timeZone` means the schedule is in that zone, and
        // everything here is UTC.
        if let Some(tz) = cj["spec"]["timeZone"].as_str() {
            warn!(
                "CronJob {namespace}/{cj_name} sets timeZone={tz}, which is not \
                 implemented — the schedule is being read as UTC"
            );
        }

        let due = match apimachinery::cron::start_to_run(
            schedule,
            last_schedule,
            created,
            now,
            deadline,
        ) {
            Ok(d) => d,
            Err(e) => {
                // Refused rather than guessed. Move the marker to now so the
                // next pass is a normal one instead of repeating the refusal
                // forever.
                warn!("CronJob {namespace}/{cj_name}: cannot catch up ({e:?}); \
                       resuming from now without running the missed starts");
                self.recorder
                    .event(
                        cj,
                        "Warning",
                        "TooManyMissedTimes",
                        &format!(
                            "Cannot determine the missed start times ({e:?}); resuming \
                             from now. Set startingDeadlineSeconds to bound how far back \
                             the controller looks."
                        ),
                    )
                    .await;
                let mut updated_cj = cj.clone();
                if updated_cj["status"].is_null() {
                    updated_cj["status"] = json!({});
                }
                updated_cj["status"]["lastScheduleTime"] =
                    json!(now.format("%Y-%m-%dT%H:%M:%SZ").to_string());
                let _ = self
                    .api
                    .update_status(
                        &format!("/apis/batch/v1/namespaces/{namespace}/cronjobs/{cj_name}"),
                        &updated_cj,
                    )
                    .await;
                None
            }
        };

        if let Some(scheduled_for) = due {
            {
                match concurrency {
                    "Forbid" => {
                        if !active_jobs.is_empty() {
                            debug!("CronJob {cj_name}: skipping (Forbid, active jobs exist)");
                            self.recorder
                                .event(
                                    cj,
                                    "Normal",
                                    "MissSchedule",
                                    &format!(
                                        "Skipped the {scheduled_for} run: concurrencyPolicy \
                                         is Forbid and {} job(s) are still active",
                                        active_jobs.len()
                                    ),
                                )
                                .await;
                        } else {
                            self.create_job(namespace, cj_name, cj_uid, cj).await?;
                        }
                    }
                    "Replace" => {
                        // Delete active jobs first
                        for job in &active_jobs {
                            let job_name = job["metadata"]["name"].as_str().unwrap_or("");
                            if !job_name.is_empty() {
                                let _ = self
                                    .api
                                    .delete(&format!(
                                        "/apis/batch/v1/namespaces/{namespace}/jobs/{job_name}"
                                    ))
                                    .await;
                            }
                        }
                        self.create_job(namespace, cj_name, cj_uid, cj).await?;
                        self.recorder
                            .event(
                                cj,
                                "Normal",
                                "SuccessfulCreate",
                                &format!(
                                    "Replaced the running job and started the \
                                     {scheduled_for} run"
                                ),
                            )
                            .await;
                    }
                    _ => {
                        // Allow
                        self.create_job(namespace, cj_name, cj_uid, cj).await?;
                        // Says which scheduled time this run is for, not just
                        // that a job appeared: a catch-up run and an on-time
                        // one are indistinguishable otherwise, and telling
                        // them apart is most of what the history is for.
                        self.recorder
                            .event(
                                cj,
                                "Normal",
                                "SuccessfulCreate",
                                &format!("Started the job for the {scheduled_for} run"),
                            )
                            .await;
                    }
                }

                // The *scheduled* instant, not the wall clock. Recording
                // "now" makes a catch-up run look like an on-time one and
                // shifts the next window, so a controller that is a few
                // seconds late slowly drifts off the schedule.
                let mut updated_cj = cj.clone();
                let now_str = scheduled_for.format("%Y-%m-%dT%H:%M:%SZ").to_string();
                if updated_cj["status"].is_null() {
                    updated_cj["status"] = json!({});
                }
                updated_cj["status"]["lastScheduleTime"] = json!(now_str);

                // Update active refs
                let active_refs: Vec<Value> = active_jobs
                    .iter()
                    .filter_map(|j| {
                        let name = j["metadata"]["name"].as_str()?;
                        let uid = j["metadata"]["uid"].as_str()?;
                        Some(json!({
                            "apiVersion": "batch/v1",
                            "kind": "Job",
                            "name": name,
                            "namespace": namespace,
                            "uid": uid
                        }))
                    })
                    .collect();
                updated_cj["status"]["active"] = json!(active_refs);

                // `lastSuccessfulTime` is upstream's field.
                if let Some(t) = last_completion(&successful_jobs) {
                    updated_cj["status"]["lastSuccessfulTime"] = json!(t);
                }

                let _ = self
                    .api
                    .update_status(
                        &format!("/apis/batch/v1/namespaces/{namespace}/cronjobs/{cj_name}"),
                        &updated_cj,
                    )
                    .await;
            }
        }

        // Clean up history, counting what left the window.
        let pruned_ok = self
            .cleanup_history(namespace, &mut successful_jobs, successful_limit)
            .await;
        let pruned_bad = self
            .cleanup_history(namespace, &mut failed_jobs, failed_limit)
            .await;

        self.write_stats(
            namespace,
            cj_name,
            cj,
            active_jobs.len() as u64,
            successful_jobs.len() as u64 - pruned_ok,
            failed_jobs.len() as u64 - pruned_bad,
            pruned_ok,
            pruned_bad,
        )
        .await;

        Ok(())
    }

    /// Lifetime counters for a CronJob.
    ///
    /// **Exact rather than sampled.** Counting the Jobs that currently exist
    /// would make the totals *fall* as `successfulJobsHistoryLimit` trims them
    /// — a CronJob that has run ten thousand times would report three. So the
    /// counters are "what is retained right now" plus "everything ever pruned",
    /// and pruning is the only way a Job leaves the window.
    ///
    /// Under `storm.io/` because they are not upstream fields. A client that
    /// does not know them ignores them; `oc get cronjob -o yaml` shows them
    /// without any client changes, which is the point.
    #[allow(clippy::too_many_arguments)]
    async fn write_stats(
        &self,
        namespace: &str,
        cj_name: &str,
        cj: &Value,
        active: u64,
        retained_ok: u64,
        retained_bad: u64,
        pruned_ok: u64,
        pruned_bad: u64,
    ) {
        let prior = &cj["status"]["storm.io/stats"];
        let ever_pruned_ok = prior["prunedSucceeded"].as_u64().unwrap_or(0) + pruned_ok;
        let ever_pruned_bad = prior["prunedFailed"].as_u64().unwrap_or(0) + pruned_bad;

        let succeeded = retained_ok + ever_pruned_ok;
        let failed = retained_bad + ever_pruned_bad;
        let total = succeeded + failed;

        let mut updated = cj.clone();
        if updated["status"].is_null() {
            updated["status"] = json!({});
        }
        updated["status"]["storm.io/stats"] = json!({
            "succeeded": succeeded,
            "failed": failed,
            "active": active,
            // Kept so the sum above stays exact across restarts.
            "prunedSucceeded": ever_pruned_ok,
            "prunedFailed": ever_pruned_bad,
            // A rate is what someone actually looks at, and computing it here
            // means every client gets the same number.
            "failureRate": if total == 0 { 0.0 } else { failed as f64 / total as f64 },
        });

        let _ = self
            .api
            .update_status(
                &format!("/apis/batch/v1/namespaces/{namespace}/cronjobs/{cj_name}"),
                &updated,
            )
            .await;
    }

    async fn create_job(
        &self,
        namespace: &str,
        cj_name: &str,
        cj_uid: &str,
        cj: &Value,
    ) -> anyhow::Result<()> {
        let timestamp = chrono::Utc::now().timestamp();
        let job_name = format!("{cj_name}-{timestamp}");

        let job_template = &cj["spec"]["jobTemplate"];
        let job = json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": {
                "name": job_name,
                "namespace": namespace,
                "labels": job_template["metadata"]["labels"].clone(),
                "ownerReferences": [{
                    "apiVersion": "batch/v1",
                    "kind": "CronJob",
                    "name": cj_name,
                    "uid": cj_uid,
                    "controller": true,
                    "blockOwnerDeletion": true
                }]
            },
            "spec": job_template["spec"]
        });

        match self
            .api
            .create(
                &format!("/apis/batch/v1/namespaces/{namespace}/jobs"),
                &job,
            )
            .await
        {
            Ok(_) => {
                info!("CronJob {cj_name}: created Job {namespace}/{job_name}");
            }
            Err(e) => {
                warn!("CronJob {cj_name}: failed to create Job: {e}");
            }
        }

        Ok(())
    }

    /// Trim the retained Jobs to the history limit, returning how many were
    /// removed.
    ///
    /// The count matters: pruning is the **only** way a Job leaves the
    /// window, so it is the one place an exact lifetime counter can be
    /// maintained. Counting the Jobs that happen to still exist would make
    /// the totals fall as history is trimmed.
    async fn cleanup_history(
        &self,
        namespace: &str,
        jobs: &mut Vec<&Value>,
        limit: usize,
    ) -> u64 {
        if jobs.len() <= limit {
            return 0;
        }
        // Sort by creation timestamp (oldest first)
        jobs.sort_by(|a, b| {
            let ta = a["metadata"]["creationTimestamp"].as_str().unwrap_or("");
            let tb = b["metadata"]["creationTimestamp"].as_str().unwrap_or("");
            ta.cmp(tb)
        });
        let to_delete = jobs.len() - limit;
        for job in jobs.iter().take(to_delete) {
            let job_name = job["metadata"]["name"].as_str().unwrap_or("");
            if !job_name.is_empty() {
                let _ = self
                    .api
                    .delete(&format!(
                        "/apis/batch/v1/namespaces/{namespace}/jobs/{job_name}"
                    ))
                    .await;
            }
        }
        to_delete as u64
    }
}

/// The latest `status.completionTime` among a set of Jobs.
fn last_completion(jobs: &[&Value]) -> Option<String> {
    jobs.iter()
        .filter_map(|j| j["status"]["completionTime"].as_str())
        .max()
        .map(str::to_owned)
}

// --- Cron parser ---

/// Check if the current time matches a 5-field cron schedule.
fn cron_matches(schedule: &str, now: &chrono::DateTime<chrono::Utc>) -> bool {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }

    let minute = now.format("%M").to_string().parse::<u32>().unwrap_or(0);
    let hour = now.format("%H").to_string().parse::<u32>().unwrap_or(0);
    let day = now.format("%d").to_string().parse::<u32>().unwrap_or(1);
    let month = now.format("%m").to_string().parse::<u32>().unwrap_or(1);
    let weekday = now.format("%u").to_string().parse::<u32>().unwrap_or(1); // 1=Mon, 7=Sun

    let minute_set = parse_cron_field(fields[0], 0, 59);
    let hour_set = parse_cron_field(fields[1], 0, 23);
    let day_set = parse_cron_field(fields[2], 1, 31);
    let month_set = parse_cron_field(fields[3], 1, 12);
    let weekday_set = parse_cron_field(fields[4], 0, 7); // 0 and 7 both = Sunday

    // Map weekday: chrono uses 1=Mon..7=Sun, cron uses 0=Sun..6=Sat (and 7=Sun)
    let cron_weekday = if weekday == 7 { 0 } else { weekday };

    minute_set.contains(&minute)
        && hour_set.contains(&hour)
        && day_set.contains(&day)
        && month_set.contains(&month)
        && (weekday_set.contains(&cron_weekday) || weekday_set.contains(&7u32) && cron_weekday == 0)
}

/// Parse a single cron field into a set of matching values.
fn parse_cron_field(field: &str, min: u32, max: u32) -> HashSet<u32> {
    let mut result = HashSet::new();
    for part in field.split(',') {
        let part = part.trim();
        if part == "*" {
            result.extend(min..=max);
        } else if let Some(step_str) = part.strip_prefix("*/") {
            if let Ok(step) = step_str.parse::<u32>() {
                if step > 0 {
                    let mut val = min;
                    while val <= max {
                        result.insert(val);
                        val += step;
                    }
                }
            }
        } else if part.contains('-') {
            let range_parts: Vec<&str> = part.split('-').collect();
            if range_parts.len() == 2 {
                if let (Ok(start), Ok(end)) = (
                    range_parts[0].parse::<u32>(),
                    range_parts[1].parse::<u32>(),
                ) {
                    result.extend(start..=end);
                }
            }
        } else if let Ok(val) = part.parse::<u32>() {
            result.insert(val);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cron_every_minute() {
        let now = chrono::Utc::now();
        assert!(cron_matches("* * * * *", &now));
    }

    #[test]
    fn test_cron_field_step() {
        let set = parse_cron_field("*/5", 0, 59);
        assert!(set.contains(&0));
        assert!(set.contains(&5));
        assert!(set.contains(&10));
        assert!(!set.contains(&3));
    }

    #[test]
    fn test_cron_field_range() {
        let set = parse_cron_field("1-5", 0, 59);
        assert!(set.contains(&1));
        assert!(set.contains(&5));
        assert!(!set.contains(&0));
        assert!(!set.contains(&6));
    }

    #[test]
    fn test_cron_field_list() {
        let set = parse_cron_field("1,3,5", 0, 59);
        assert!(set.contains(&1));
        assert!(set.contains(&3));
        assert!(set.contains(&5));
        assert!(!set.contains(&2));
    }
}
