//! In-memory watch cache (the "cacher").
//!
//! Upstream kube-apiserver never opens one etcd watch per client. It keeps a
//! single watch per resource type and fans out to every client watcher in
//! memory, replaying recent events from a ring buffer so a client that watches
//! from a recent `resourceVersion` needs no new store watch.
//!
//! This module does the same over the `KvStore` (fastetcd): one upstream
//! `store.watch(prefix, …)` per prefix, a bounded ring of recent events, and a
//! broadcast fan-out to all watchers. A client whose requested revision predates
//! what the pump captured falls back to a dedicated store watch (correctness
//! over sharing for cold/old revisions).

use apimachinery::store::{KvStore, WatchStream};
use apimachinery::watch::WatchEvent;
use apimachinery::Result;
use dashmap::DashMap;
use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

/// The resource name inside a store prefix (`/registry/pods/` -> `pods`), for
/// the metric labels.
fn resource_of_prefix(prefix: &str) -> String {
    crate::storage::metric_resource(prefix)
}

/// The watch event kind, as upstream labels it.
fn event_kind(ev: &WatchEvent) -> &'static str {
    match ev {
        WatchEvent::Added { .. } => "ADDED",
        WatchEvent::Modified { .. } => "MODIFIED",
        WatchEvent::Deleted { .. } => "DELETED",
        WatchEvent::Bookmark { .. } => "BOOKMARK",
        WatchEvent::Error { .. } => "ERROR",
    }
}

/// How long a LIST waits for the snapshot to reach a revision this apiserver
/// wrote, before reading the store instead.
const MIN_REV_WAIT: std::time::Duration = std::time::Duration::from_millis(50);
/// Page size used to seed the snapshot from the store.
const SEED_PAGE: usize = 1000;
/// How often the freshness task re-checks the snapshot against the store. Bounds
/// how long a LIST can be stale if the upstream watch silently stalls.
const FRESHNESS_SECS: u64 = 5;
/// A prefix silent this long is re-checked against the store (stall vs quiet).
const STALL_SECS: u64 = 30;

/// Read the full prefix from the store into a key→object map, returning it with
/// the revision it reflects. Pages through the whole prefix.
/// Key → (object bytes, the revision that last wrote it). The revision is the
/// object's resourceVersion; a LIST item without it cannot be compared with
/// anything (#111).
type Snapshot = BTreeMap<String, (Vec<u8>, u64)>;

/// A page of a LIST from the cache (or, when it lags, from the store).
pub struct CachePage {
    /// Each object with its own mod revision.
    pub items: Vec<(Vec<u8>, u64)>,
    /// The last key returned, when more follow.
    pub continue_key: Option<String>,
    /// The revision the page reflects.
    pub revision: u64,
    /// How many keys follow this page, when known (upstream's
    /// `remainingItemCount`).
    pub remaining: Option<u64>,
}

async fn seed_snapshot(store: &Arc<dyn KvStore>, prefix: &str) -> Result<(Snapshot, u64)> {
    let mut snapshot = BTreeMap::new();
    let mut continue_token: Option<String> = None;
    let mut rev = 0u64;
    loop {
        let page = store
            .list_at(
                prefix,
                SEED_PAGE,
                continue_token.as_deref(),
                if rev == 0 { None } else { Some(rev) },
            )
            .await?;
        rev = page.revision;
        for (key, bytes, mod_rev) in page.items {
            snapshot.insert(key, (bytes, mod_rev));
        }
        match page.continue_token {
            Some(t) => continue_token = Some(t),
            None => break,
        }
    }
    Ok((snapshot, rev))
}

/// Recent events retained per prefix for replay to newly-attaching watchers.
const RING_CAPACITY: usize = 1024;
/// Live broadcast backlog per prefix (a slow watcher beyond this lags).
const BROADCAST_CAPACITY: usize = 1024;
/// Per-client channel depth.
const CLIENT_CHANNEL: usize = 256;

/// Monotonic per-prefix sequence, used to dedup the ring/live overlap window.
type Seq = u64;

struct PrefixCache {
    /// Live fan-out to all current watchers of this prefix.
    tx: broadcast::Sender<(Seq, WatchEvent)>,
    /// Bounded ring of recent events (with their sequence) for replay.
    ring: Mutex<VecDeque<(Seq, WatchEvent)>>,
    /// Next sequence to assign.
    next_seq: AtomicU64,
    /// Store revision the pump started after — events with `revision >
    /// pump_start_rev` are captured; anything at/below is not reconstructable.
    pump_start_rev: u64,
    /// Materialized key→object snapshot (seeded from the store, kept current by
    /// the pump) so LIST/relist storms are served from memory, not the store.
    snapshot: Mutex<Snapshot>,
    /// Revision the snapshot currently reflects.
    snapshot_rev: AtomicU64,
    advanced: tokio::sync::Notify,
    terminated: AtomicBool,
    /// Last time the pump made progress (an event) or the freshness task
    /// re-seeded. Used to distinguish a *quiet* prefix from a *stalled* watch.
    last_progress: Mutex<std::time::Instant>,
}

/// One shared watch cache over a `KvStore`, keyed by resource prefix.
pub struct WatchCache {
    store: Arc<dyn KvStore>,
    caches: Arc<DashMap<String, Arc<PrefixCache>>>,
    /// Serializes cache creation so a prefix gets exactly one upstream pump.
    init_lock: tokio::sync::Mutex<()>,
}

impl WatchCache {
    pub fn new(store: Arc<dyn KvStore>) -> Self {
        Self {
            store,
            caches: Arc::new(DashMap::new()),
            init_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Ensure a single upstream pump exists for `prefix`, returning its cache.
    async fn ensure(&self, prefix: &str) -> Result<Arc<PrefixCache>> {
        if let Some(c) = self.caches.get(prefix) {
            if !c.terminated.load(Ordering::SeqCst) {
                return Ok(c.clone());
            }
        }
        // Only one creator at a time; re-check under the lock (another task may
        // have created it while we waited).
        let _guard = self.init_lock.lock().await;
        if let Some(c) = self.caches.get(prefix) {
            if !c.terminated.load(Ordering::SeqCst) {
                return Ok(c.clone());
            }
        }

        // Seed a full key→object snapshot from the store (paging through the
        // whole prefix). The pump then keeps it current, so LISTs never hit the
        // store again for this prefix. `start_rev` is the seed's revision.
        let (snapshot, start_rev) = seed_snapshot(&self.store, prefix).await?;

        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let cache = Arc::new(PrefixCache {
            tx,
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            next_seq: AtomicU64::new(1),
            pump_start_rev: start_rev,
            snapshot: Mutex::new(snapshot),
            snapshot_rev: AtomicU64::new(start_rev),
            advanced: tokio::sync::Notify::new(),
            terminated: AtomicBool::new(false),
            last_progress: Mutex::new(std::time::Instant::now()),
        });

        // Single upstream watch for this prefix.
        let mut stream = self.store.watch(prefix, start_rev + 1).await?;
        self.caches.insert(prefix.to_string(), cache.clone());
        let pump = cache.clone();
        let caches = self.caches.clone();
        let prefix_owned = prefix.to_string();
        let prefix_metric = prefix.to_string();
        let pump_task = tokio::spawn(async move {
            while let Some(mut ev) = stream.recv().await {
                let seq = pump.next_seq.fetch_add(1, Ordering::SeqCst);
                if matches!(ev, WatchEvent::Error { .. }) {
                    let _ = pump.tx.send((seq, ev));
                    break;
                }
                // Keep the materialized snapshot current.
                {
                    let mut snap = pump.snapshot.lock().unwrap();
                    match &mut ev {
                        WatchEvent::Added {
                            key,
                            value,
                            revision,
                        } => {
                            snap.insert(key.clone(), (value.clone(), *revision));
                        }
                        // The state it replaces goes with the event, so a
                        // selector watch can tell "stopped matching" (#67).
                        WatchEvent::Modified {
                            key,
                            value,
                            prev_value,
                            revision,
                        } => {
                            let last = snap
                                .insert(key.clone(), (value.clone(), *revision))
                                .map(|(v, _)| v);
                            if prev_value.is_none() {
                                *prev_value = last;
                            }
                        }
                        // What is removed is the object's last state: hand it
                        // to the event, before the ring and the fan-out see
                        // it, so every watcher's DELETED carries the object
                        // rather than a name (#100).
                        WatchEvent::Deleted {
                            key, prev_value, ..
                        } => {
                            let last = snap.remove(key).map(|(v, _)| v);
                            if prev_value.is_none() {
                                *prev_value = last;
                            }
                        }
                        WatchEvent::Bookmark { .. } | WatchEvent::Error { .. } => {}
                    }
                }
                pump.snapshot_rev.store(ev.revision(), Ordering::SeqCst);
                pump.advanced.notify_waiters();
                *pump.last_progress.lock().unwrap() = std::time::Instant::now();

                // What the cache holds, under upstream's names. These are
                // free here — the numbers already exist — and they are the
                // ones that answer "is the cache keeping up" and "how much is
                // in this cluster", which otherwise take a LIST to find out.
                let resource = resource_of_prefix(&prefix_metric);
                metrics::counter!(
                    "apiserver_watch_events_total",
                    "resource" => resource.clone(),
                    "kind" => event_kind(&ev),
                )
                .increment(1);
                metrics::gauge!("apiserver_storage_objects", "resource" => resource.clone())
                    .set(pump.snapshot.lock().unwrap().len() as f64);
                metrics::gauge!("watch_cache_capacity", "resource" => resource)
                    .set(RING_CAPACITY as f64);
                {
                    let mut ring = pump.ring.lock().unwrap();
                    if ring.len() == RING_CAPACITY {
                        ring.pop_front();
                    }
                    ring.push_back((seq, ev.clone()));
                }
                // Err just means no live subscribers right now; the ring still
                // retains the event for replay.
                let _ = pump.tx.send((seq, ev));
            }
            // Upstream watch ended — drop the prefix so the next watcher re-opens.
            pump.terminated.store(true, Ordering::SeqCst);
            pump.advanced.notify_waiters();
            let seq = pump.next_seq.fetch_add(1, Ordering::SeqCst);
            let _ = pump.tx.send((
                seq,
                WatchEvent::Error {
                    code: 503,
                    message: "datastore watch disconnected".into(),
                    revision: pump.snapshot_rev.load(Ordering::SeqCst),
                },
            ));
            caches.remove_if(&prefix_owned, |_, value| Arc::ptr_eq(value, &pump));
        });

        // Freshness task: if the upstream watch *silently stalls* (connection
        // alive but no events), the pump above never notices and the snapshot
        // freezes → stale LISTs (rustkube#18). Periodically compare the snapshot
        // revision to the store's current revision for this prefix; if behind,
        // re-seed. Bounds staleness to FRESHNESS_SECS and self-heals a stall.
        {
            let fresh = cache.clone();
            let store = self.store.clone();
            let caches = self.caches.clone();
            let prefix_fresh = prefix.to_string();
            let abort_pump = pump_task.abort_handle();
            tokio::spawn(async move {
                let mut tick =
                    tokio::time::interval(std::time::Duration::from_secs(FRESHNESS_SECS));
                tick.tick().await; // consume the immediate first tick
                let stall = std::time::Duration::from_secs(STALL_SECS);
                loop {
                    tick.tick().await;
                    // Stop once this prefix's pump has been torn down.
                    if !caches
                        .get(&prefix_fresh)
                        .is_some_and(|entry| Arc::ptr_eq(entry.value(), &fresh))
                    {
                        break;
                    }
                    // Only suspect a stall if the pump has made NO progress for a
                    // while — a healthy watch delivers events (a busy prefix) or
                    // the cluster is simply idle. This avoids re-seeding quiet
                    // prefixes just because other prefixes advanced the store's
                    // global revision.
                    if fresh.last_progress.lock().unwrap().elapsed() < stall {
                        continue;
                    }
                    let cur_rev = match store.list(&prefix_fresh, 1, None).await {
                        Ok(p) => p.revision,
                        Err(_) => continue,
                    };
                    if fresh.snapshot_rev.load(Ordering::SeqCst) < cur_rev {
                        if let Ok((snap, rev)) = seed_snapshot(&store, &prefix_fresh).await {
                            // Did anything actually change?
                            //
                            // `store.list` returns the store's *global*
                            // revision, so a prefix nobody has written looks
                            // behind the moment anything else is written. On
                            // an idle cluster that is every empty prefix,
                            // forever, and each one warned once per stall
                            // window: over a thousand WARN lines in ten
                            // minutes, led by
                            //
                            //   watch-cache: re-seeded
                            //   prefix=/registry/horizontalpodautoscalers/…
                            //
                            // for resources the cluster does not have a single
                            // one of. The comment above already said "stalled
                            // **or long-quiet**", and only the first of those
                            // is worth a warning: a re-seed that changes
                            // nothing means the watch missed nothing.
                            let changed = {
                                let mut held = fresh.snapshot.lock().unwrap();
                                let changed = *held != snap;
                                *held = snap;
                                changed
                            };
                            fresh.snapshot_rev.store(rev, Ordering::SeqCst);
                            fresh.advanced.notify_waiters();
                            // Count the re-seed as progress so a persistently
                            // quiet prefix re-seeds at most once per STALL window.
                            *fresh.last_progress.lock().unwrap() = std::time::Instant::now();
                            if changed {
                                // Missing watch events require client relists
                                // and a new upstream watch, not silent cache
                                // replacement under existing subscriptions.
                                fresh.terminated.store(true, Ordering::SeqCst);
                                fresh.advanced.notify_waiters();
                                let seq = fresh.next_seq.fetch_add(1, Ordering::SeqCst);
                                let _ = fresh.tx.send((
                                    seq,
                                    WatchEvent::Error {
                                        code: 410,
                                        message: "watch cache resynchronized after missed events"
                                            .into(),
                                        revision: rev,
                                    },
                                ));
                                caches.remove_if(&prefix_fresh, |_, value| {
                                    Arc::ptr_eq(value, &fresh)
                                });
                                abort_pump.abort();
                                tracing::warn!(
                                    "watch-cache: re-seeded prefix={prefix_fresh} to rev={rev} — the watch had missed events"
                                );
                            } else {
                                tracing::debug!(
                                    "watch-cache: prefix={prefix_fresh} caught up to rev={rev}, unchanged"
                                );
                            }
                        }
                    }
                }
            });
        }

        tracing::info!(
            "watch-cache: opened upstream watch for prefix={prefix} from rev={start_rev}"
        );
        Ok(cache)
    }

    /// LIST `prefix` from the in-memory snapshot (seeded once from the store,
    /// then kept current by the pump), paginated by key. Continue tokens are the
    /// last returned key, so all pages are served consistently from the cache —
    /// a relist storm hits the store only once (the seed), not once per client.
    /// Returns `(items, continue_token, revision)`.
    /// A page of `prefix` from the snapshot, once the snapshot reflects
    /// revision `min_rev` (0: no requirement).
    ///
    /// The pump applies an event milliseconds after the write, so the wait is
    /// notified as soon as the pump advances. If it has not caught up within
    /// `MIN_REV_WAIT`, the page is read from the store instead: slower, and
    /// never stale.
    pub async fn list(
        &self,
        prefix: &str,
        limit: usize,
        continue_token: Option<&str>,
        min_rev: u64,
    ) -> Result<CachePage> {
        let cache = self.ensure(prefix).await?;
        if !wait_for_revision(&cache, min_rev).await {
            tracing::debug!("watch-cache: {prefix} not at rev {min_rev} after {MIN_REV_WAIT:?}; listing from the store");
            let page = self.store.list(prefix, limit, continue_token).await?;
            let items = page.items.into_iter().map(|(_, v, r)| (v, r)).collect();
            return Ok(CachePage {
                items,
                continue_key: page.continue_token,
                revision: page.revision,
                remaining: None,
            });
        }
        let snap = cache.snapshot.lock().unwrap();
        let rev = cache.snapshot_rev.load(Ordering::SeqCst);

        // Resume strictly after the previous page's last key.
        let lower = match continue_token {
            Some(k) => Bound::Excluded(k.to_string()),
            None => Bound::Unbounded,
        };
        let mut items = Vec::new();
        let mut last_key: Option<String> = None;
        let mut next: Option<String> = None;
        let mut remaining = None;
        let mut range = snap.range((lower, Bound::Unbounded));
        for (key, value) in range.by_ref() {
            items.push(value.clone());
            last_key = Some(key.clone());
            if limit != 0 && items.len() == limit {
                break;
            }
        }
        // Anything left is the next page: resume after the last key returned
        // (Excluded bound above), and say how much is left.
        let left = range.count() as u64;
        if left > 0 {
            next = last_key;
            remaining = Some(left);
        }
        Ok(CachePage {
            items,
            continue_key: next,
            revision: rev,
            remaining,
        })
    }

    /// Watch `prefix` for events after `start_rev`, served from the shared cache
    /// when the revision is recent, else from a dedicated store watch.
    pub async fn watch(&self, prefix: &str, start_rev: u64) -> Result<WatchStream> {
        let cache = self.ensure(prefix).await?;

        // Revision 0 is "from now", as it is to etcd — not "from the
        // beginning". The cache's live stream from where it stands is exactly
        // that, so it is served here rather than by a store watch of its own.
        // It used to fall through to the store, which is every watch a client
        // opens without a resourceVersion: one more upstream watch each, and
        // DELETED events without the last state only the cache holds (#100).
        let start_rev = if start_rev == 0 {
            cache.snapshot_rev.load(Ordering::SeqCst)
        } else {
            start_rev
        };

        // Subscribe before taking a replay snapshot; inspect the replay
        // boundary under the same lock so an eviction cannot open a gap.
        let mut live = cache.tx.subscribe();
        let (ring_gap, backlog): (bool, Vec<(Seq, WatchEvent)>) = {
            let ring = cache.ring.lock().unwrap();
            let gap = ring
                .front()
                .is_some_and(|(seq, event)| *seq > 1 && event.revision() > start_rev);
            (
                gap,
                ring.iter()
                    .filter(|(_, event)| event.revision() > start_rev)
                    .cloned()
                    .collect(),
            )
        };
        if cache.terminated.load(Ordering::SeqCst) || start_rev < cache.pump_start_rev || ring_gap {
            // The datastore either replays the full suffix or returns a
            // terminal 410. Never silently skip an evicted segment.
            return self.store.watch(prefix, start_rev.saturating_add(1)).await;
        }
        let (tx, rx) = mpsc::channel(CLIENT_CHANNEL);
        tokio::spawn(async move {
            let mut last_seq = 0u64;
            for (seq, ev) in backlog {
                last_seq = seq;
                if tx.send(ev).await.is_err() {
                    return;
                }
            }
            loop {
                let received = tokio::select! {
                    _ = tx.closed() => return,
                    received = live.recv() => received,
                };
                match received {
                    Ok((seq, ev)) => {
                        if matches!(ev, WatchEvent::Error { .. }) {
                            let _ = tx.send(ev).await;
                            return;
                        }
                        // Skip anything already sent from the ring (seq) or below
                        // the client's requested revision.
                        if seq > last_seq && ev.revision() > start_rev {
                            if tx.send(ev).await.is_err() {
                                return;
                            }
                        }
                    }
                    // Slow client fell behind the broadcast buffer. It may miss
                    // events; upstream clients relist on watch gaps, so continue.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = tx
                            .send(WatchEvent::Error {
                                code: 410,
                                message: "watch consumer fell behind retained history".into(),
                                revision: start_rev,
                            })
                            .await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(rx)
    }
}

/// Subscribe before checking, including when several LISTs await one write.
async fn wait_for_revision(cache: &PrefixCache, min_rev: u64) -> bool {
    let deadline = tokio::time::Instant::now() + MIN_REV_WAIT;
    loop {
        let advanced = cache.advanced.notified();
        tokio::pin!(advanced);
        advanced.as_mut().enable();
        if cache.terminated.load(Ordering::SeqCst) {
            return false;
        }
        if cache.snapshot_rev.load(Ordering::SeqCst) >= min_rev {
            return true;
        }
        if tokio::time::timeout_at(deadline, advanced).await.is_err() {
            return cache.snapshot_rev.load(Ordering::SeqCst) >= min_rev;
        }
    }
}

#[cfg(test)]
mod revision_tests {
    use super::*;
    fn cache() -> Arc<PrefixCache> {
        Arc::new(PrefixCache {
            tx: broadcast::channel(16).0,
            ring: Mutex::new(VecDeque::new()),
            next_seq: AtomicU64::new(1),
            pump_start_rev: 1,
            snapshot: Mutex::new(BTreeMap::new()),
            snapshot_rev: AtomicU64::new(1),
            advanced: tokio::sync::Notify::new(),
            terminated: AtomicBool::new(false),
            last_progress: Mutex::new(std::time::Instant::now()),
        })
    }
    #[tokio::test]
    async fn advancing_revision_wakes_all_waiters() {
        let cache = cache();
        let first = cache.clone();
        let second = cache.clone();
        let a = tokio::spawn(async move { wait_for_revision(&first, 2).await });
        let b = tokio::spawn(async move { wait_for_revision(&second, 2).await });
        tokio::task::yield_now().await;
        cache.snapshot_rev.store(2, Ordering::SeqCst);
        cache.advanced.notify_waiters();
        assert!(a.await.unwrap());
        assert!(b.await.unwrap());
        assert!(wait_for_revision(&cache, 2).await);
    }
    #[tokio::test]
    async fn a_stalled_watch_releases_the_reader_for_store_fallback() {
        assert!(!wait_for_revision(&cache(), 2).await);
    }
}
