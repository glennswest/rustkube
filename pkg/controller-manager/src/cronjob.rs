//! CronJob controller.
//!
//! Creates Jobs on a cron schedule. Supports standard 5-field cron expressions
//! (minute, hour, day-of-month, month, day-of-week). Handles concurrency
//! policies (Allow, Forbid, Replace) and history limits.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, info, warn};

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
        info!("CronJob indexed object workers started");
        crate::owned::run(&self.api, self).await;
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
        // The CronJob as last written this pass. A pass can write status more
        // than once — the schedule marker, then the stats — and a status PUT
        // is conditional on the resourceVersion it carries (#78), so each
        // write starts from what the previous one stored. Built from `cj`,
        // the stats write 409'd after a scheduling write; before #78 it
        // landed and put back the old `lastScheduleTime`.
        let mut latest = cj.clone();
        let schedule = cj["spec"]["schedule"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("cronjob missing schedule"))?;
        let concurrency = cj["spec"]["concurrencyPolicy"].as_str().unwrap_or("Allow");
        let suspend = cj["spec"]["suspend"].as_bool().unwrap_or(false);
        let successful_limit = cj["spec"]["successfulJobsHistoryLimit"]
            .as_u64()
            .unwrap_or(3) as usize;
        let failed_limit = cj["spec"]["failedJobsHistoryLimit"].as_u64().unwrap_or(1) as usize;

        if suspend {
            return Ok(());
        }

        // Find jobs owned by this CronJob
        let owned_jobs: Vec<&Value> = all_jobs.iter().filter(|job| owned_by(job, cj_uid)).collect();

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
                        c["type"].as_str() == Some("Failed") && c["status"].as_str() == Some("True")
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
        // Not "does the schedule match this instant": the controller wakes at
        // the next scheduled start (the requeue below) and on changes, and a
        // wake can be late, so asking only about *now* loses every run the
        // controller was not awake for — a
        // restart, a slow reconcile, a node that was paused. The window since
        // the last run is what gets evaluated, and only the most recent missed
        // start is acted on.
        let now = chrono::Utc::now();
        if let Ok(next) = apimachinery::cron::next_start(schedule, now) {
            apimachinery::reactor::requeue_at_time(next);
        }
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

        let due =
            match apimachinery::cron::start_to_run(schedule, last_schedule, created, now, deadline)
            {
                Ok(d) => d,
                Err(e) => {
                    // Refused rather than guessed. Move the marker to now so the
                    // next pass is a normal one instead of repeating the refusal
                    // forever.
                    warn!(
                        "CronJob {namespace}/{cj_name}: cannot catch up ({e:?}); \
                       resuming from now without running the missed starts"
                    );
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
                    let mut updated_cj = latest.clone();
                    if updated_cj["status"].is_null() {
                        updated_cj["status"] = json!({});
                    }
                    updated_cj["status"]["lastScheduleTime"] =
                        json!(now.format("%Y-%m-%dT%H:%M:%SZ").to_string());
                    let resp = self
                        .api
                        .update_status(
                            &format!("/apis/batch/v1/namespaces/{namespace}/cronjobs/{cj_name}"),
                            &updated_cj,
                        )
                        .await;
                    if let Some(stored) = written(resp) {
                        latest = stored;
                    }
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
                            self.create_job(namespace, cj_name, cj_uid, cj, scheduled_for)
                                .await?;
                        }
                    }
                    "Replace" => {
                        // Delete active jobs first
                        for job in &active_jobs {
                            let job_name = job["metadata"]["name"].as_str().unwrap_or("");
                            if !job_name.is_empty() {
                                let _ = self
                                    .api
                                    .delete_observed(
                                        &format!(
                                            "/apis/batch/v1/namespaces/{namespace}/jobs/{job_name}"
                                        ),
                                        job,
                                    )
                                    .await;
                            }
                        }
                        if self
                            .create_job(namespace, cj_name, cj_uid, cj, scheduled_for)
                            .await?
                        {
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
                    }
                    _ => {
                        // Allow
                        // Says which scheduled time this run is for, not just
                        // that a job appeared: a catch-up run and an on-time
                        // one are indistinguishable otherwise, and telling
                        // them apart is most of what the history is for.
                        if self
                            .create_job(namespace, cj_name, cj_uid, cj, scheduled_for)
                            .await?
                        {
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
                }

                // The *scheduled* instant, not the wall clock. Recording
                // "now" makes a catch-up run look like an on-time one and
                // shifts the next window, so a controller that is a few
                // seconds late slowly drifts off the schedule.
                let mut updated_cj = latest.clone();
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

                let resp = self
                    .api
                    .update_status(
                        &format!("/apis/batch/v1/namespaces/{namespace}/cronjobs/{cj_name}"),
                        &updated_cj,
                    )
                    .await;
                if let Some(stored) = written(resp) {
                    latest = stored;
                }
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
            &latest,
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

    /// Create the Job for the run scheduled at `scheduled_for`, answering
    /// whether this call made it (`false`: it was already there).
    ///
    /// An error is returned, not logged and swallowed: the caller records
    /// `lastScheduleTime` only after this succeeds, so a refused or failed
    /// create leaves the run owed and the pass fails into the runner's
    /// backoff, which tries it again (#364). Swallowed, a transient apiserver
    /// or datastore failure consumed the scheduled run with no Job made.
    ///
    /// The name is the scheduled minute, as upstream's (`<cronjob>-<unix
    /// minutes>`), so one run has one name: a create that committed while
    /// its status write failed answers AlreadyExists on the retry, and that
    /// Job — when it is this CronJob's — is the run, not a reason to make a
    /// second one.
    async fn create_job(
        &self,
        namespace: &str,
        cj_name: &str,
        cj_uid: &str,
        cj: &Value,
        scheduled_for: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<bool> {
        let job_name = job_name(cj_name, scheduled_for);
        let path = format!("/apis/batch/v1/namespaces/{namespace}/jobs");

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

        match self.api.create(&path, &job).await {
            Ok(_) => {
                info!("CronJob {cj_name}: created Job {namespace}/{job_name}");
                Ok(true)
            }
            Err(e) if apimachinery::api_status::status_of(&e).map(|s| s.as_u16()) == Some(409) => {
                let response = self.api.get(&format!("{path}/{job_name}")).await?;
                let existing: Value = apimachinery::api_status::check(response).await?.json().await?;
                anyhow::ensure!(
                    owned_by(&existing, cj_uid),
                    "CronJob {namespace}/{cj_name}: Job {job_name} for the {scheduled_for} run \
                     exists and is not this CronJob's"
                );
                info!("CronJob {cj_name}: Job {namespace}/{job_name} already exists for the {scheduled_for} run");
                Ok(false)
            }
            Err(e) => {
                warn!("CronJob {cj_name}: failed to create Job {namespace}/{job_name}: {e}");
                Err(e.context(format!(
                    "CronJob {namespace}/{cj_name}: creating the Job for the {scheduled_for} run"
                )))
            }
        }
    }

    /// Trim the retained Jobs to the history limit, returning how many were
    /// removed.
    ///
    /// The count matters: pruning is the **only** way a Job leaves the
    /// window, so it is the one place an exact lifetime counter can be
    /// maintained. Counting the Jobs that happen to still exist would make
    /// the totals fall as history is trimmed.
    async fn cleanup_history(&self, namespace: &str, jobs: &mut Vec<&Value>, limit: usize) -> u64 {
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
                    .delete_observed(
                        &format!("/apis/batch/v1/namespaces/{namespace}/jobs/{job_name}"),
                        job,
                    )
                    .await;
            }
        }
        to_delete as u64
    }
}

/// The latest `status.completionTime` among a set of Jobs.
/// The object a status PUT stored, when it stored one.
///
/// A refused write answers with a `Status` (a 409 among them), which is not a
/// base for the next write; the caller keeps what it had, and the next pass
/// starts from a fresh list.
fn written(resp: anyhow::Result<Value>) -> Option<Value> {
    let v = resp.ok()?;
    (v["kind"].as_str() != Some("Status") && v["metadata"]["resourceVersion"].is_string())
        .then_some(v)
}

/// The Job of the run scheduled at `scheduled_for`: one name per run.
fn job_name(cj_name: &str, scheduled_for: chrono::DateTime<chrono::Utc>) -> String {
    format!("{cj_name}-{}", scheduled_for.timestamp().div_euclid(60))
}

fn owned_by(job: &Value, uid: &str) -> bool {
    job["metadata"]["ownerReferences"]
        .as_array()
        .is_some_and(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(uid)))
}

fn last_completion(jobs: &[&Value]) -> Option<String> {
    jobs.iter()
        .filter_map(|j| j["status"]["completionTime"].as_str())
        .max()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_stored_object_is_a_base_for_the_next_write() {
        let stored = json!({
            "kind": "CronJob",
            "metadata": { "name": "c", "resourceVersion": "12" },
            "status": { "lastScheduleTime": "2026-09-26T10:00:00Z" },
        });
        assert_eq!(written(Ok(stored.clone())), Some(stored));

        let conflict = json!({
            "kind": "Status", "apiVersion": "v1", "status": "Failure",
            "reason": "Conflict", "code": 409, "metadata": {},
        });
        assert_eq!(written(Ok(conflict)), None);
    }

    #[test]
    fn a_run_has_one_job_name_its_scheduled_minute() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-20T10:05:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(job_name("c", at), format!("c-{}", at.timestamp() / 60));
        assert_eq!(job_name("c", at + chrono::Duration::seconds(59)), job_name("c", at));
        assert_ne!(job_name("c", at + chrono::Duration::seconds(60)), job_name("c", at));
    }

    /// A fake apiserver for one CronJob pass: the Job create answers
    /// `create_status` (201 makes the Job), and every status PUT is kept.
    mod fake {
        use axum::{
            body::Bytes,
            extract::State,
            http::{Method, StatusCode, Uri},
            response::{IntoResponse, Response},
            Json, Router,
        };
        use serde_json::{json, Value};
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        pub struct Api {
            pub create_status: Mutex<u16>,
            pub creates: Mutex<Vec<Value>>,
            pub existing: Mutex<Option<Value>>,
            pub status_puts: Mutex<Vec<Value>>,
        }

        async fn handle(State(api): State<Arc<Api>>, method: Method, uri: Uri, body: Bytes) -> Response {
            let path = uri.path();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if method == Method::POST && path.ends_with("/jobs") {
                api.creates.lock().unwrap().push(body.clone());
                let code = *api.create_status.lock().unwrap();
                if code == 201 {
                    *api.existing.lock().unwrap() = Some(body.clone());
                    return (StatusCode::CREATED, Json(body)).into_response();
                }
                let reason = if code == 409 { "AlreadyExists" } else { "ServiceUnavailable" };
                let status = json!({"kind":"Status","apiVersion":"v1","status":"Failure",
                    "reason":reason,"code":code,"message":"refused by the fake"});
                return (StatusCode::from_u16(code).unwrap(), Json(status)).into_response();
            }
            if method == Method::GET && path.contains("/jobs/") {
                return match api.existing.lock().unwrap().clone() {
                    Some(job) => Json(job).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                };
            }
            if method == Method::PUT && path.ends_with("/status") {
                api.status_puts.lock().unwrap().push(body.clone());
                let mut stored = body;
                stored["metadata"]["resourceVersion"] = json!("2");
                return Json(stored).into_response();
            }
            // Events and anything else: accepted.
            (StatusCode::CREATED, Json(json!({"kind":"Event","metadata":{"resourceVersion":"1"}}))).into_response()
        }

        pub async fn serve(api: Arc<Api>) -> String {
            let app = Router::new().fallback(handle).with_state(api);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            format!("http://{addr}")
        }
    }

    /// A minutely CronJob made three minutes ago with no run yet: one start
    /// is owed.
    fn owed_cronjob() -> Value {
        let created = chrono::Utc::now() - chrono::Duration::minutes(3);
        json!({
            "apiVersion": "batch/v1", "kind": "CronJob",
            "metadata": {
                "name": "c", "namespace": "ns", "uid": "cj-uid", "resourceVersion": "1",
                "creationTimestamp": created.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            },
            "spec": {
                "schedule": "* * * * *",
                "jobTemplate": {"spec": {"template": {"spec": {"containers": [{"name": "x", "image": "x"}]}}}},
            },
        })
    }

    /// The distinct `lastScheduleTime`s written (the stats write repeats the
    /// one the scheduling write stored).
    fn scheduled(puts: &[Value]) -> Vec<String> {
        let mut marks: Vec<String> = puts
            .iter()
            .filter_map(|p| p["status"]["lastScheduleTime"].as_str().map(str::to_owned))
            .collect();
        marks.dedup();
        marks
    }

    #[tokio::test]
    async fn a_failed_job_create_leaves_the_run_owed_and_the_retry_makes_it() {
        let api = Arc::new(fake::Api::default());
        *api.create_status.lock().unwrap() = 503;
        let url = fake::serve(api.clone()).await;
        let controller = CronJobController::new(Arc::new(ApiClient::new(&url)));
        let cj = owed_cronjob();

        let failed = controller.reconcile_cronjob("ns", &cj, &[]).await;
        assert!(failed.is_err(), "a refused Job create must fail the pass");
        assert_eq!(api.creates.lock().unwrap().len(), 1);
        assert!(
            scheduled(&api.status_puts.lock().unwrap()).is_empty(),
            "lastScheduleTime must not advance for a run with no Job"
        );

        // The apiserver recovers; the retried pass makes the same run's Job.
        *api.create_status.lock().unwrap() = 201;
        controller.reconcile_cronjob("ns", &cj, &[]).await.unwrap();
        let creates = api.creates.lock().unwrap().clone();
        assert_eq!(creates.len(), 2);
        assert_eq!(creates[0]["metadata"]["name"], creates[1]["metadata"]["name"]);
        let marks = scheduled(&api.status_puts.lock().unwrap());
        assert_eq!(marks.len(), 1, "one scheduling write, for the run made");
        let at = chrono::DateTime::parse_from_rfc3339(&marks[0]).unwrap().with_timezone(&chrono::Utc);
        assert_eq!(creates[1]["metadata"]["name"], json!(job_name("c", at)));
    }

    #[tokio::test]
    async fn an_existing_job_of_this_cronjob_is_the_run() {
        let api = Arc::new(fake::Api::default());
        *api.create_status.lock().unwrap() = 409;
        let url = fake::serve(api.clone()).await;
        let controller = CronJobController::new(Arc::new(ApiClient::new(&url)));
        let cj = owed_cronjob();
        *api.existing.lock().unwrap() = Some(json!({
            "kind": "Job",
            "metadata": {"name": "whatever", "ownerReferences": [{"kind": "CronJob", "uid": "cj-uid"}]},
        }));

        controller.reconcile_cronjob("ns", &cj, &[]).await.unwrap();
        assert_eq!(scheduled(&api.status_puts.lock().unwrap()).len(), 1);
    }

    #[tokio::test]
    async fn an_existing_job_of_someone_else_is_not_the_run() {
        let api = Arc::new(fake::Api::default());
        *api.create_status.lock().unwrap() = 409;
        let url = fake::serve(api.clone()).await;
        let controller = CronJobController::new(Arc::new(ApiClient::new(&url)));
        let cj = owed_cronjob();
        *api.existing.lock().unwrap() = Some(json!({
            "kind": "Job",
            "metadata": {"name": "whatever", "ownerReferences": [{"kind": "CronJob", "uid": "other"}]},
        }));

        assert!(controller.reconcile_cronjob("ns", &cj, &[]).await.is_err());
        assert!(scheduled(&api.status_puts.lock().unwrap()).is_empty());
    }
}

#[async_trait::async_trait]
impl crate::owned::Controller for CronJobController {
    fn name(&self) -> &'static str {
        "cronjob"
    }
    fn primary(&self) -> &'static str {
        "/apis/batch/v1/cronjobs"
    }
    fn children(&self) -> Option<&'static str> {
        Some("/apis/batch/v1/jobs")
    }
    async fn reconcile(
        &self,
        object: &Value,
        children: &[Value],
        _deps: &crate::owned::Deps,
    ) -> anyhow::Result<()> {
        let namespace = object["metadata"]["namespace"]
            .as_str()
            .unwrap_or("default");
        self.reconcile_cronjob(namespace, object, children).await
    }
}
