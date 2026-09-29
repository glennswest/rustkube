//! Shared indexed collection feeds. The last subscriber cancels its watch.
use crate::informer::{Delta, Index, Key, Store};
use crate::reflector::Change;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

type Callback = Arc<dyn Fn(&[Delta], bool) + Send + Sync>;
static NEXT: AtomicU64 = AtomicU64::new(1);

struct Inner {
    feeds: Mutex<HashMap<String, Arc<Feed>>>,
}
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}
impl Default for Hub {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                feeds: Mutex::new(HashMap::new()),
            }),
        }
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        for feed in self.feeds.get_mut().unwrap().values() {
            if let Some(task) = feed.task.lock().unwrap().take() {
                task.abort();
            }
        }
    }
}
pub struct Feed {
    url: String,
    store: Mutex<Store>,
    subscribers: Mutex<HashMap<u64, Callback>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    relist: Arc<tokio::sync::Notify>,
}
impl Feed {
    pub fn ensure_synced(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.store.lock().unwrap().is_synced(),
            "informer is not synchronized"
        );
        Ok(())
    }

    pub fn key_for_uid(&self, uid: &str) -> Option<Key> {
        self.store.lock().unwrap().key_for_uid(uid)
    }
    pub fn get(&self, key: &Key) -> anyhow::Result<Option<Value>> {
        self.store.lock().unwrap().get(key).map(|v| v.cloned())
    }
    pub fn select(&self, index: &Index) -> anyhow::Result<Vec<Value>> {
        self.store.lock().unwrap().select(index)
    }
    /// Every object; fails while unsynchronized rather than reading empty.
    pub fn list(&self) -> anyhow::Result<Vec<Value>> {
        self.store.lock().unwrap().values()
    }
    pub fn keys(&self) -> anyhow::Result<Vec<Key>> {
        self.store.lock().unwrap().keys()
    }
    fn notify(&self, changes: &[Delta], reset: bool) {
        let changes = changes
            .iter()
            .filter(|change| match (&change.old, &change.new) {
                (Some(old), Some(new)) => {
                    crate::reflector::semantic(old.clone())
                        != crate::reflector::semantic(new.clone())
                }
                _ => true,
            })
            .cloned()
            .collect::<Vec<_>>();
        // A bookmark or a revision-only echo changes nothing. Subscribers
        // that wake on any call (discovery, pending Pods) would otherwise
        // run on every 45 s heartbeat bookmark: a poll loop in disguise.
        if changes.is_empty() && !reset {
            return;
        }
        let callbacks = self
            .subscribers
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for callback in callbacks {
            callback(&changes, reset);
        }
    }
    /// False when the observation could not be applied: the store is then
    /// unsynchronized and the reflector relists to repair it.
    fn receive(&self, event: Change) -> bool {
        let summary = match &event {
            Change::Applied { event, .. } => Some(format!(
                "{} {}/{} uid={} rv={}",
                event["type"].as_str().unwrap_or("?"),
                event["object"]["metadata"]["namespace"]
                    .as_str()
                    .unwrap_or(""),
                event["object"]["metadata"]["name"].as_str().unwrap_or("?"),
                event["object"]["metadata"]["uid"].as_str().unwrap_or("-"),
                event["object"]["metadata"]["resourceVersion"]
                    .as_str()
                    .unwrap_or("-"),
            )),
            _ => None,
        };
        let (changes, reset) = {
            let mut store = self.store.lock().unwrap();
            match event {
                Change::Reset { snapshot, started } => {
                    (store.reset_started_at(&snapshot, started), true)
                }
                Change::Applied {
                    event,
                    semantic_change,
                } => {
                    let result = store.apply(&event).map(|change| {
                        if semantic_change {
                            change.into_iter().collect()
                        } else {
                            Vec::new()
                        }
                    });
                    (result, false)
                }
                Change::Unavailable => {
                    store.unavailable();
                    return true;
                }
                Change::Connected => {
                    // Only a recovery is a reset: work that failed while the
                    // feed was unsynchronized needs waking. A routine
                    // reconnect resumes from its revision with nothing
                    // missed; resetting there requeued every object on
                    // every watch timeout, a periodic resync.
                    let recovering = !store.is_synced();
                    store.resumed();
                    if !recovering {
                        return true;
                    }
                    (Ok(Vec::new()), true)
                }
            }
        };
        match changes {
            Ok(changes) => {
                self.notify(&changes, reset);
                true
            }
            Err(error) => {
                self.store.lock().unwrap().unavailable();
                tracing::warn!(url = %self.url, %error, event = summary.as_deref().unwrap_or("snapshot"),
                    "indexed informer rejected observation; relisting");
                false
            }
        }
    }
}

pub struct Subscription {
    pub feed: Arc<Feed>,
    id: u64,
    url: String,
    hub: Weak<Inner>,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let mut feeds = hub.feeds.lock().unwrap();
        let mut subscribers = self.feed.subscribers.lock().unwrap();
        subscribers.remove(&self.id);
        if subscribers.is_empty() {
            if let Some(task) = self.feed.task.lock().unwrap().take() {
                task.abort();
            }
            feeds.remove(&self.url);
        }
    }
}

impl Hub {
    pub fn subscribe(
        &self,
        client: &reqwest::Client,
        url: String,
        callback: impl Fn(&[Delta], bool) + Send + Sync + 'static,
    ) -> Subscription {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let callback: Callback = Arc::new(callback);
        let feed = {
            let mut feeds = self.inner.feeds.lock().unwrap();
            let feed = feeds
                .entry(url.clone())
                .or_insert_with(|| {
                    let feed = Arc::new(Feed {
                        url: url.clone(),
                        store: Mutex::new(Store::default()),
                        subscribers: Mutex::new(HashMap::new()),
                        task: Mutex::new(None),
                        relist: Arc::new(tokio::sync::Notify::new()),
                    });
                    let weak = Arc::downgrade(&feed);
                    let client = client.clone();
                    let url = url.clone();
                    let relist = feed.relist.clone();
                    let task = tokio::spawn(async move {
                        loop {
                            let weak = weak.clone();
                            tokio::select! {
                                _ = crate::reflector::run_events(client.clone(), url.clone(), move |event| {
                                    weak.upgrade().is_none_or(|feed| feed.receive(event))
                                }) => return,
                                _ = relist.notified() => {},
                            }
                        }
                    });
                    *feed.task.lock().unwrap() = Some(task);
                    feed
                })
                .clone();
            feed.subscribers
                .lock()
                .unwrap()
                .insert(id, callback.clone());
            feed
        };
        // A subscriber joining an already-synchronized feed must get its seed.
        let seed = feed.store.lock().unwrap().values();
        if let Ok(objects) = seed {
            let changes: Vec<_> = objects
                .into_iter()
                .map(|object| Delta {
                    old: None,
                    new: Some(object),
                    affected: Default::default(),
                })
                .collect();
            callback(&changes, true);
        }
        Subscription {
            feed,
            id,
            url,
            hub: Arc::downgrade(&self.inner),
        }
    }

    /// Called for every successful object write before returning to the caller.
    pub fn acknowledge(&self, url: &str, object: &Value, started: std::time::Instant) {
        let Some(collection) = collection_url(url, object) else {
            return;
        };
        let feed = self.inner.feeds.lock().unwrap().get(&collection).cloned();
        if let Some(feed) = feed {
            let change = feed
                .store
                .lock()
                .unwrap()
                .acknowledge_since(object.clone(), started);
            match change {
                Ok(Some(change)) => feed.notify(&[change], false),
                Ok(None) => {}
                Err(error) => {
                    feed.store.lock().unwrap().unavailable();
                    feed.relist.notify_one();
                    tracing::warn!(%error,"cannot record successful write; relisting informer");
                }
            }
        }
    }
}

/// Normalize namespaced writes to the all-namespaces collection feed.
fn collection_url(url: &str, object: &Value) -> Option<String> {
    object["metadata"]["name"].as_str()?;
    let mut url = reqwest::Url::parse(url).ok()?;
    let mut segments = url.path_segments()?.map(str::to_string).collect::<Vec<_>>();
    let offset = if segments.first()?.as_str() == "api" {
        2
    } else {
        3
    };
    if segments.get(offset).is_some_and(|s| s == "namespaces") && segments.len() > offset + 2 {
        segments.drain(offset..offset + 2);
    }
    segments.truncate(offset + 1);
    url.set_path(&format!("/{}", segments.join("/")));
    url.set_query(None);
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acknowledged_noop_status_does_not_create_a_reconcile_loop() {
        let feed = Feed {
            url: "test".into(),
            store: Mutex::new(Store::default()),
            subscribers: Mutex::new(HashMap::new()),
            task: Mutex::new(None),
            relist: Arc::new(tokio::sync::Notify::new()),
        };
        let count = Arc::new(AtomicU64::new(0));
        let observed = count.clone();
        feed.subscribers.lock().unwrap().insert(
            1,
            Arc::new(move |changes, _| {
                observed.fetch_add(changes.len() as u64, Ordering::Relaxed);
            }),
        );
        let object = serde_json::json!({"metadata":{"name":"pod","uid":"uid","resourceVersion":"a"},"status":{"phase":"Running"}});
        feed.receive(Change::Reset {
            snapshot: serde_json::json!({"items":[object.clone()]}),
            started: std::time::Instant::now(),
        });
        let mut updated = object;
        updated["metadata"]["resourceVersion"] = serde_json::json!("b");
        let change = feed
            .store
            .lock()
            .unwrap()
            .acknowledge(updated)
            .unwrap()
            .unwrap();
        feed.notify(&[change], false);
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn namespaced_status_and_create_writes_find_the_shared_feed() {
        let object = serde_json::json!({"metadata":{"name":"web"}});
        for path in [
            "/apis/apps/v1/namespaces/ns/replicasets/web/status",
            "/apis/apps/v1/namespaces/ns/replicasets",
        ] {
            assert_eq!(
                collection_url(&format!("https://api{path}"), &object).unwrap(),
                "https://api/apis/apps/v1/replicasets"
            );
        }
        let object = serde_json::json!({"metadata":{"name":"pods"}});
        assert_eq!(
            collection_url("https://api/api/v1/namespaces/ns/pods", &object).unwrap(),
            "https://api/api/v1/pods"
        );
    }
}
