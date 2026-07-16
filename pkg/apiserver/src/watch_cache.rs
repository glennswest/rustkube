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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

/// Page size used to seed the snapshot from the store.
const SEED_PAGE: usize = 1000;
/// How often the freshness task re-checks the snapshot against the store. Bounds
/// how long a LIST can be stale if the upstream watch silently stalls.
const FRESHNESS_SECS: u64 = 5;
/// A prefix silent this long is re-checked against the store (stall vs quiet).
const STALL_SECS: u64 = 30;

/// Read the full prefix from the store into a key→object map, returning it with
/// the revision it reflects. Pages through the whole prefix.
async fn seed_snapshot(
    store: &Arc<dyn KvStore>,
    prefix: &str,
) -> Result<(BTreeMap<String, Vec<u8>>, u64)> {
    let mut snapshot = BTreeMap::new();
    let mut continue_token: Option<String> = None;
    let mut rev = 0u64;
    loop {
        let page = store.list(prefix, SEED_PAGE, continue_token.as_deref()).await?;
        rev = page.revision;
        for (key, bytes, _rev) in page.items {
            snapshot.insert(key, bytes);
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
    snapshot: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Revision the snapshot currently reflects.
    snapshot_rev: AtomicU64,
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
            return Ok(c.clone());
        }
        // Only one creator at a time; re-check under the lock (another task may
        // have created it while we waited).
        let _guard = self.init_lock.lock().await;
        if let Some(c) = self.caches.get(prefix) {
            return Ok(c.clone());
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
            last_progress: Mutex::new(std::time::Instant::now()),
        });

        // Single upstream watch for this prefix.
        let mut stream = self.store.watch(prefix, start_rev + 1).await?;
        let pump = cache.clone();
        let caches = self.caches.clone();
        let prefix_owned = prefix.to_string();
        tokio::spawn(async move {
            while let Some(ev) = stream.recv().await {
                let seq = pump.next_seq.fetch_add(1, Ordering::SeqCst);
                // Keep the materialized snapshot current.
                {
                    let mut snap = pump.snapshot.lock().unwrap();
                    match &ev {
                        WatchEvent::Added { key, value, .. }
                        | WatchEvent::Modified { key, value, .. } => {
                            snap.insert(key.clone(), value.clone());
                        }
                        WatchEvent::Deleted { key, .. } => {
                            snap.remove(key);
                        }
                        WatchEvent::Bookmark { .. } => {}
                    }
                }
                pump.snapshot_rev.store(ev.revision(), Ordering::SeqCst);
                *pump.last_progress.lock().unwrap() = std::time::Instant::now();
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
            caches.remove(&prefix_owned);
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
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(FRESHNESS_SECS));
                tick.tick().await; // consume the immediate first tick
                let stall = std::time::Duration::from_secs(STALL_SECS);
                loop {
                    tick.tick().await;
                    // Stop once this prefix's pump has been torn down.
                    if !caches.contains_key(&prefix_fresh) {
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
                            *fresh.snapshot.lock().unwrap() = snap;
                            fresh.snapshot_rev.store(rev, Ordering::SeqCst);
                            // Count the re-seed as progress so a persistently
                            // quiet prefix re-seeds at most once per STALL window.
                            *fresh.last_progress.lock().unwrap() = std::time::Instant::now();
                            tracing::warn!(
                                "watch-cache: re-seeded prefix={prefix_fresh} to rev={rev} (watch stalled or long-quiet)"
                            );
                        }
                    }
                }
            });
        }

        self.caches.insert(prefix.to_string(), cache.clone());
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
    pub async fn list(
        &self,
        prefix: &str,
        limit: usize,
        continue_token: Option<&str>,
    ) -> Result<(Vec<Vec<u8>>, Option<String>, u64)> {
        let cache = self.ensure(prefix).await?;
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
        for (key, value) in snap.range((lower, Bound::Unbounded)) {
            if limit != 0 && items.len() == limit {
                // A further item exists → resume the next page after the last
                // key we actually returned (Excluded bound above).
                next = last_key.clone();
                break;
            }
            items.push(value.clone());
            last_key = Some(key.clone());
        }
        Ok((items, next, rev))
    }

    /// Watch `prefix` for events after `start_rev`, served from the shared cache
    /// when the revision is recent, else from a dedicated store watch.
    pub async fn watch(&self, prefix: &str, start_rev: u64) -> Result<WatchStream> {
        let cache = self.ensure(prefix).await?;

        // The pump captured only `revision > pump_start_rev`. If the client
        // wants events from before that, the cache can't reconstruct them —
        // fall back to a dedicated store watch (old behavior for stale RVs,
        // including watch-from-0).
        if start_rev < cache.pump_start_rev {
            tracing::debug!(
                "watch-cache: fallback store watch prefix={prefix} start_rev={start_rev} < pump_start={}",
                cache.pump_start_rev
            );
            return self.store.watch(prefix, start_rev).await;
        }
        tracing::debug!("watch-cache: serving prefix={prefix} from shared cache");

        // Subscribe to live BEFORE snapshotting the ring so no event slips
        // through the gap between the two; dedup the overlap by sequence.
        let mut live = cache.tx.subscribe();
        let backlog: Vec<(Seq, WatchEvent)> = {
            let ring = cache.ring.lock().unwrap();
            ring.iter()
                .filter(|(_, e)| e.revision() > start_rev)
                .cloned()
                .collect()
        };

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
                match live.recv().await {
                    Ok((seq, ev)) => {
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
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(rx)
    }
}
