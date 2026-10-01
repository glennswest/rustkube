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
    /// Unique per pump: a re-opened prefix is a new generation, so a reader
    /// keyed on `(generation, revision)` never mistakes one for the other.
    generation: u64,
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
    last_progress: Mutex<tokio::time::Instant>,
}

/// Source of `PrefixCache::generation`.
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// What a snapshot reflects: the pump's generation and the store revision.
/// Two reads with the same version saw the same objects.
pub type SnapshotVersion = (u64, u64);

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
            generation: GENERATION.fetch_add(1, Ordering::SeqCst),
            tx,
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            next_seq: AtomicU64::new(1),
            pump_start_rev: start_rev,
            snapshot: Mutex::new(snapshot),
            snapshot_rev: AtomicU64::new(start_rev),
            advanced: tokio::sync::Notify::new(),
            terminated: AtomicBool::new(false),
            last_progress: Mutex::new(tokio::time::Instant::now()),
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
                *pump.last_progress.lock().unwrap() = tokio::time::Instant::now();

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
                            *fresh.last_progress.lock().unwrap() = tokio::time::Instant::now();
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

    /// The version of `prefix`'s snapshot, opening its pump on first use.
    ///
    /// Two atomic loads once the pump exists: cheap enough to ask on every
    /// request, so a reader can keep its own parsed copy of a prefix and
    /// rebuild it only when this changes (the RBAC authorizer, #177).
    pub async fn version(&self, prefix: &str) -> Result<SnapshotVersion> {
        let cache = self.ensure(prefix).await?;
        Ok((cache.generation, cache.snapshot_rev.load(Ordering::SeqCst)))
    }

    /// Every object under `prefix` as the cache holds it, keyed by store key,
    /// with the version it reflects. The pump applies a write milliseconds
    /// after the datastore accepts it, so this can trail a write that has
    /// just been acknowledged; a caller that cannot accept that reads the
    /// store.
    pub async fn snapshot(&self, prefix: &str) -> Result<(SnapshotVersion, Vec<(String, Vec<u8>)>)> {
        let cache = self.ensure(prefix).await?;
        let snap = cache.snapshot.lock().unwrap();
        // Read under the lock: the pump writes the snapshot before it stores
        // the revision, so this version is never newer than these objects.
        let version = (cache.generation, cache.snapshot_rev.load(Ordering::SeqCst));
        Ok((version, snap.iter().map(|(k, (v, _))| (k.clone(), v.clone())).collect()))
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
        if !wait_for_revision(&cache, min_rev, MIN_REV_WAIT).await {
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

/// Wait until the snapshot reflects `min_rev`, for at most `budget`: true when
/// it does, false when the budget runs out or the cache is torn down (the
/// caller then reads the store). Woken by `advanced`, never by polling.
///
/// The `Notified` future is enabled *before* the state is checked, so an
/// advance between the check and the await still wakes it (`notify_waiters`
/// stores no permit). Dropping the future — a client that goes away — just
/// deregisters it.
async fn wait_for_revision(cache: &PrefixCache, min_rev: u64, budget: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
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
    use apimachinery::store::{LeaseId, ListResult};
    use async_trait::async_trait;
    use std::time::Duration;
    use tokio::time::Instant;

    const P: &str = "/registry/pods/";

    /// A store whose revision and contents the test sets, and whose watch
    /// delivers only the events the test sends: so a page tells whether it
    /// came from the cache (which saw the events) or the store (which did not).
    #[derive(Default)]
    struct ScriptedStore {
        state: Mutex<(u64, BTreeMap<String, (Vec<u8>, u64)>)>,
        watchers: Mutex<Vec<mpsc::Sender<WatchEvent>>>,
    }

    impl ScriptedStore {
        fn at(rev: u64, keys: &[&str]) -> Arc<Self> {
            let store = Self::default();
            for key in keys {
                store.set(rev, key);
            }
            store.state.lock().unwrap().0 = rev;
            Arc::new(store)
        }
        /// Write `key` at `rev` in the store only — no watch event.
        fn set(&self, rev: u64, key: &str) {
            let mut state = self.state.lock().unwrap();
            state.0 = rev;
            state
                .1
                .insert(key.to_string(), (key.as_bytes().to_vec(), rev));
        }
        /// Advance the store's revision without touching this prefix.
        fn bump(&self, rev: u64) {
            self.state.lock().unwrap().0 = rev;
        }
        /// Deliver an event to the open watch, as the datastore would.
        async fn emit(&self, key: &str, rev: u64) {
            let tx = self.watchers.lock().unwrap().last().cloned().unwrap();
            tx.send(WatchEvent::Added {
                key: key.to_string(),
                value: key.as_bytes().to_vec(),
                revision: rev,
            })
            .await
            .unwrap();
        }
        /// End every open watch, as a lost datastore connection does.
        fn close_watches(&self) {
            self.watchers.lock().unwrap().clear();
        }
    }

    #[async_trait]
    impl KvStore for ScriptedStore {
        async fn get(&self, _: &str) -> Result<Option<(Vec<u8>, u64)>> {
            unimplemented!()
        }
        async fn put(&self, _: &str, _: &[u8], _: Option<u64>) -> Result<u64> {
            unimplemented!()
        }
        async fn delete(&self, _: &str, _: Option<u64>) -> Result<u64> {
            unimplemented!()
        }
        async fn list(&self, prefix: &str, _: usize, _: Option<&str>) -> Result<ListResult> {
            let state = self.state.lock().unwrap();
            Ok(ListResult {
                items: state
                    .1
                    .iter()
                    .filter(|(k, _)| k.starts_with(prefix))
                    .map(|(k, (v, r))| (k.clone(), v.clone(), *r))
                    .collect(),
                continue_token: None,
                revision: state.0,
                remaining: None,
            })
        }
        async fn watch(&self, _: &str, _: u64) -> Result<WatchStream> {
            let (tx, rx) = mpsc::channel(64);
            self.watchers.lock().unwrap().push(tx);
            Ok(rx)
        }
        async fn lease_grant(&self, _: Duration) -> Result<LeaseId> {
            unimplemented!()
        }
        async fn lease_keepalive(&self, _: LeaseId) -> Result<()> {
            unimplemented!()
        }
        async fn lease_revoke(&self, _: LeaseId) -> Result<()> {
            unimplemented!()
        }
        async fn compact(&self, _: u64) -> Result<()> {
            unimplemented!()
        }
    }

    fn keys(page: &CachePage) -> Vec<String> {
        page.items
            .iter()
            .map(|(v, _)| String::from_utf8(v.clone()).unwrap())
            .collect()
    }

    /// A cache over `store`, opened (seeded, pump running).
    async fn opened(store: &Arc<ScriptedStore>) -> Arc<WatchCache> {
        let wc = Arc::new(WatchCache::new(store.clone()));
        wc.list(P, 0, None, 0).await.unwrap();
        wc
    }

    fn spawn_list(
        wc: &Arc<WatchCache>,
        min_rev: u64,
    ) -> tokio::task::JoinHandle<(CachePage, Duration)> {
        let wc = wc.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let page = wc.list(P, 0, None, min_rev).await.unwrap();
            (page, started.elapsed())
        })
    }

    fn cache() -> Arc<PrefixCache> {
        Arc::new(PrefixCache {
            generation: 0,
            tx: broadcast::channel(16).0,
            ring: Mutex::new(VecDeque::new()),
            next_seq: AtomicU64::new(1),
            pump_start_rev: 1,
            snapshot: Mutex::new(BTreeMap::new()),
            snapshot_rev: AtomicU64::new(1),
            advanced: tokio::sync::Notify::new(),
            terminated: AtomicBool::new(false),
            last_progress: Mutex::new(Instant::now()),
        })
    }

    #[tokio::test]
    async fn advancing_revision_wakes_all_waiters() {
        let cache = cache();
        let first = cache.clone();
        let second = cache.clone();
        let a = tokio::spawn(async move { wait_for_revision(&first, 2, MIN_REV_WAIT).await });
        let b = tokio::spawn(async move { wait_for_revision(&second, 2, MIN_REV_WAIT).await });
        tokio::task::yield_now().await;
        cache.snapshot_rev.store(2, Ordering::SeqCst);
        cache.advanced.notify_waiters();
        assert!(a.await.unwrap());
        assert!(b.await.unwrap());
        assert!(wait_for_revision(&cache, 2, MIN_REV_WAIT).await);
    }

    #[tokio::test]
    async fn a_stalled_watch_releases_the_reader_for_store_fallback() {
        assert!(!wait_for_revision(&cache(), 2, MIN_REV_WAIT).await);
    }

    // Paused time: a waiter that missed its wakeup would sleep out the whole
    // budget, which the clock then shows exactly.

    #[tokio::test(start_paused = true)]
    async fn the_pump_applying_the_write_wakes_a_waiting_list() {
        let store = ScriptedStore::at(5, &["/registry/pods/a"]);
        let wc = opened(&store).await;
        let waiter = spawn_list(&wc, 6);
        tokio::task::yield_now().await;
        store.emit("/registry/pods/b", 6).await;
        let (page, waited) = waiter.await.unwrap();
        assert!(waited < MIN_REV_WAIT, "woken by the pump, not the deadline");
        assert_eq!(page.revision, 6);
        // Only the cache saw the event: this page is the snapshot.
        assert_eq!(keys(&page), ["/registry/pods/a", "/registry/pods/b"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_revision_the_pump_never_reaches_falls_back_to_the_store_at_the_deadline() {
        let store = ScriptedStore::at(5, &["/registry/pods/a"]);
        let wc = opened(&store).await;
        store.set(7, "/registry/pods/c");
        let (page, waited) = spawn_list(&wc, 7).await.unwrap();
        // Timer deadlines round to the millisecond.
        assert!(waited >= MIN_REV_WAIT && waited <= MIN_REV_WAIT + Duration::from_millis(2));
        assert_eq!(page.revision, 7);
        assert_eq!(keys(&page), ["/registry/pods/a", "/registry/pods/c"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_datastore_watch_releases_the_waiter_at_once() {
        let store = ScriptedStore::at(5, &["/registry/pods/a"]);
        let wc = opened(&store).await;
        let mut events = wc.watch(P, 0).await.unwrap();
        let waiter = spawn_list(&wc, 9);
        tokio::task::yield_now().await;
        store.set(9, "/registry/pods/d");
        store.close_watches();
        let (page, waited) = waiter.await.unwrap();
        assert_eq!(waited, Duration::ZERO, "woken by the pump ending");
        assert_eq!(page.revision, 9);
        assert_eq!(keys(&page), ["/registry/pods/a", "/registry/pods/d"]);
        assert!(matches!(
            events.recv().await,
            Some(WatchEvent::Error { code: 503, .. })
        ));
    }

    /// The stall check re-seeds at the first freshness tick at or past
    /// `STALL_SECS` of silence: 30 s after the cache opened.
    const JUST_BEFORE_RESEED: Duration = Duration::from_millis(STALL_SECS * 1000 - 30);

    #[tokio::test(start_paused = true)]
    async fn an_unchanged_reseed_advances_the_revision_and_wakes_the_waiter() {
        let store = ScriptedStore::at(5, &["/registry/pods/a"]);
        let wc = opened(&store).await;
        let mut events = wc.watch(P, 0).await.unwrap();
        // Another prefix moved the global revision; this one is just quiet.
        store.bump(8);
        tokio::time::sleep(JUST_BEFORE_RESEED).await;
        let (page, waited) = spawn_list(&wc, 8).await.unwrap();
        assert!(
            waited > Duration::ZERO && waited < MIN_REV_WAIT,
            "woken by the re-seed at ~30 ms, got {waited:?}"
        );
        assert_eq!(page.revision, 8);
        assert_eq!(keys(&page), ["/registry/pods/a"]);
        // Nothing was missed, so watchers carry on.
        assert!(events.try_recv().is_err());
        assert!(!wc.caches.get(P).unwrap().terminated.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn a_reseed_that_finds_missed_events_wakes_the_waiter_and_ends_watches() {
        let store = ScriptedStore::at(5, &["/registry/pods/a"]);
        let wc = opened(&store).await;
        let mut events = wc.watch(P, 0).await.unwrap();
        // Written, but the datastore watch never said so.
        store.set(8, "/registry/pods/e");
        tokio::time::sleep(JUST_BEFORE_RESEED).await;
        let (page, waited) = spawn_list(&wc, 8).await.unwrap();
        assert!(
            waited > Duration::ZERO && waited < MIN_REV_WAIT,
            "woken by the re-seed at ~30 ms, got {waited:?}"
        );
        assert_eq!(page.revision, 8);
        assert_eq!(keys(&page), ["/registry/pods/a", "/registry/pods/e"]);
        assert!(matches!(
            events.recv().await,
            Some(WatchEvent::Error { code: 410, .. })
        ));
        assert!(wc.caches.get(P).is_none(), "the next reader re-opens");
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_waiters_do_not_disturb_later_ones() {
        let store = ScriptedStore::at(5, &["/registry/pods/a"]);
        let wc = opened(&store).await;
        // A client that disconnects mid-wait, and one that gives up.
        let gone = spawn_list(&wc, 6);
        tokio::task::yield_now().await;
        gone.abort();
        assert!(gone.await.err().unwrap().is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), wc.list(P, 0, None, 6))
                .await
                .is_err()
        );
        let waiter = spawn_list(&wc, 6);
        tokio::task::yield_now().await;
        store.emit("/registry/pods/b", 6).await;
        let (page, waited) = waiter.await.unwrap();
        assert_eq!(waited, Duration::ZERO);
        assert_eq!(keys(&page), ["/registry/pods/a", "/registry/pods/b"]);
    }

    /// Waiters registering on some threads while the revision advances on
    /// others. The budget is an hour, so a missed wakeup hangs the round and
    /// the outer timeout fails the test, instead of hiding in a fallback.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_notification_is_lost_to_a_concurrent_advance() {
        let cache = cache();
        let rounds = async {
            for rev in 2..2002u64 {
                let waiters: Vec<_> = (0..8)
                    .map(|_| {
                        let cache = cache.clone();
                        tokio::spawn(async move {
                            wait_for_revision(&cache, rev, Duration::from_secs(3600)).await
                        })
                    })
                    .collect();
                let advancer = cache.clone();
                tokio::spawn(async move {
                    advancer.snapshot_rev.store(rev, Ordering::SeqCst);
                    advancer.advanced.notify_waiters();
                });
                for waiter in waiters {
                    assert!(waiter.await.unwrap());
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), rounds)
            .await
            .expect("a waiter missed its wakeup");
    }
}
