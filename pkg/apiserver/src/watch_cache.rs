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
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

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

        // Snapshot the current store revision so the pump only captures newer
        // events instead of replaying the whole prefix history into the ring.
        let start_rev = self
            .store
            .list(prefix, 1, None)
            .await
            .map(|r| r.revision)
            .unwrap_or(0);

        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let cache = Arc::new(PrefixCache {
            tx,
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            next_seq: AtomicU64::new(1),
            pump_start_rev: start_rev,
        });

        // Single upstream watch for this prefix.
        let mut stream = self.store.watch(prefix, start_rev + 1).await?;
        let pump = cache.clone();
        let caches = self.caches.clone();
        let prefix_owned = prefix.to_string();
        tokio::spawn(async move {
            while let Some(ev) = stream.recv().await {
                let seq = pump.next_seq.fetch_add(1, Ordering::SeqCst);
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

        self.caches.insert(prefix.to_string(), cache.clone());
        tracing::info!(
            "watch-cache: opened upstream watch for prefix={prefix} from rev={start_rev}"
        );
        Ok(cache)
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
