//! Dependency-tracked workers over shared collection reflectors.
//!
//! This adapter keeps existing authoritative API reads while replacing their
//! poll clocks. Per-object indexed reconciliation can use WorkQueue directly.
use crate::workqueue::{Work, WorkQueue};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::time::Instant;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

tokio::task_local! { static CURRENT: Arc<WorkerState>; }
tokio::task_local! { static OBJECT: ObjectContext; }
struct ObjectContext {
    failed: Arc<AtomicBool>,
    requeue: Arc<dyn Fn(Duration) + Send + Sync>,
}

/// Per-object executors retain semantic deadlines and handled-error retries
/// without subscribing each object to whole-collection compatibility feeds.
pub async fn scope_object<T>(
    requeue: impl Fn(Duration) + Send + Sync + 'static,
    future: impl Future<Output = T>,
) -> (T, bool) {
    let failed = Arc::new(AtomicBool::new(false));
    let result = OBJECT
        .scope(
            ObjectContext {
                failed: failed.clone(),
                requeue: Arc::new(requeue),
            },
            future,
        )
        .await;
    (result, failed.load(Ordering::Relaxed))
}

struct Feed {
    subscribers: HashMap<u64, Weak<WorkerState>>,
    task: tokio::task::JoinHandle<()>,
}
struct HubInner {
    feeds: Mutex<HashMap<String, Feed>>,
}
impl Drop for HubInner {
    fn drop(&mut self) {
        for (_, feed) in self.feeds.get_mut().unwrap().drain() {
            feed.task.abort();
        }
    }
}

#[derive(Clone)]
pub struct WatchHub {
    inner: Arc<HubInner>,
}
impl Default for WatchHub {
    fn default() -> Self {
        Self {
            inner: Arc::new(HubInner {
                feeds: Mutex::new(HashMap::new()),
            }),
        }
    }
}
impl WatchHub {
    pub fn worker(&self, name: &'static str) -> Worker {
        let queue = WorkQueue::new();
        queue.add(());
        Worker {
            state: Arc::new(WorkerState {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                name,
                hub: self.clone(),
                queue,
                paths: Mutex::new(HashSet::new()),
                failed: AtomicBool::new(false),
                failures: Mutex::new(0),
            }),
        }
    }

    /// Called before the authoritative read, inside a worker's task scope.
    /// Shared auth client is unchanged; watch handles die with their last
    /// subscriber, including when a leader term is cancelled.
    pub fn observe(&self, client: &reqwest::Client, url: String) {
        let _ = CURRENT.try_with(|worker| {
            if !worker.paths.lock().unwrap().insert(url.clone()) {
                return;
            }
            let mut feeds = self.inner.feeds.lock().unwrap();
            let feed = feeds.entry(url.clone()).or_insert_with(|| {
                let weak = Arc::downgrade(&self.inner);
                let notify_url = url.clone();
                let client = client.clone();
                let task = tokio::spawn(async move {
                    crate::reflector::run(client, url, move || {
                        if let Some(hub) = weak.upgrade() {
                            let subscribers: Vec<_> = hub
                                .feeds
                                .lock()
                                .unwrap()
                                .get(&notify_url)
                                .map(|f| f.subscribers.values().cloned().collect())
                                .unwrap_or_default();
                            // Upgrade/drop workers outside the hub lock: the
                            // final worker drop removes its subscriptions.
                            for subscriber in subscribers {
                                if let Some(worker) = subscriber.upgrade() {
                                    worker.queue.add(());
                                }
                            }
                        }
                    })
                    .await;
                });
                Feed {
                    subscribers: HashMap::new(),
                    task,
                }
            });
            feed.subscribers.insert(worker.id, Arc::downgrade(worker));
        });
    }
}

struct WorkerState {
    id: u64,
    name: &'static str,
    hub: WatchHub,
    queue: Arc<WorkQueue<()>>,
    paths: Mutex<HashSet<String>>,
    failed: AtomicBool,
    failures: Mutex<u32>,
}
impl Drop for WorkerState {
    fn drop(&mut self) {
        let mut feeds = self.hub.inner.feeds.lock().unwrap();
        for path in self.paths.get_mut().unwrap().drain() {
            if let Some(feed) = feeds.get_mut(&path) {
                feed.subscribers.remove(&self.id);
                if feed.subscribers.is_empty() {
                    if let Some(feed) = feeds.remove(&path) {
                        feed.task.abort();
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct Worker {
    state: Arc<WorkerState>,
}
impl Worker {
    pub fn enqueue(&self) {
        self.state.queue.add(());
    }
    pub async fn next(&self) -> Work<()> {
        self.state.queue.next().await
    }

    /// Recompute semantic deadlines each pass. Events received while the
    /// future runs mark its key dirty and cannot be consumed by this pass.
    pub async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        self.state.queue.cancel_deadline(&());
        self.state.failed.store(false, Ordering::Relaxed);
        let started = Instant::now();
        let result = CURRENT.scope(self.state.clone(), future).await;
        let failed = self.state.failed.load(Ordering::Relaxed);
        let mut failures = self.state.failures.lock().unwrap();
        if failed {
            *failures = failures.saturating_add(1);
            let millis = (100_u64 << (*failures).min(8)).min(30_000);
            self.state
                .queue
                .add_at((), Instant::now() + Duration::from_millis(millis));
        } else {
            *failures = 0;
        }
        metrics::histogram!("rustkube_reconcile_duration_seconds", "controller" => self.state.name)
            .record(started.elapsed().as_secs_f64());
        metrics::counter!("rustkube_reconciles_total", "controller" => self.state.name,
            "result" => if failed { "error" } else { "success" })
        .increment(1);
        result
    }
}

/// Called by API clients even when a controller handles/logs the error itself.
pub fn failed() {
    let _ = CURRENT.try_with(|w| w.failed.store(true, Ordering::Relaxed));
    let _ = OBJECT.try_with(|w| w.failed.store(true, Ordering::Relaxed));
}

pub fn check<T, E>(result: Result<T, E>) -> Result<T, E> {
    if result.is_err() {
        failed();
    }
    result
}

/// Explicit semantic timer, never a normal idle reconcile interval.
pub fn requeue_after(delay: Duration) {
    let _ = CURRENT.try_with(|w| w.queue.add_at((), Instant::now() + delay));
    let _ = OBJECT.try_with(|w| (w.requeue)(delay));
}

pub fn requeue_at_time(timestamp: chrono::DateTime<chrono::Utc>) {
    if let Ok(delay) = (timestamp - chrono::Utc::now()).to_std() {
        requeue_after(delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn stable_worker_has_no_periodic_wakeup() {
        let hub = WatchHub::default();
        let worker = hub.worker("test");
        let work = worker.next().await;
        worker.run(async {}).await;
        drop(work);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), worker.next())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn handled_api_error_still_retries_without_external_events() {
        let hub = WatchHub::default();
        let worker = hub.worker("test");
        let work = worker.next().await;
        worker
            .run(async {
                let _ = check::<(), _>(Err("offline"));
            })
            .await;
        drop(work);
        assert!(tokio::time::timeout(Duration::from_secs(1), worker.next())
            .await
            .is_ok());
    }
}

/// Tokio task locals are not inherited by spawn; use this for bounded child
/// tasks which perform part of the same reconciliation.
pub fn inherit<T>(future: impl Future<Output = T>) -> impl Future<Output = T> {
    let current = CURRENT.try_with(Arc::clone).ok();
    async move {
        match current {
            Some(state) => CURRENT.scope(state, future).await,
            None => future.await,
        }
    }
}

pub fn requeue_deadline(start: chrono::DateTime<chrono::Utc>, seconds: u64) {
    if let Some(at) = i64::try_from(seconds)
        .ok()
        .and_then(chrono::Duration::try_seconds)
        .and_then(|delta| start.checked_add_signed(delta))
    {
        requeue_at_time(at);
    }
}
