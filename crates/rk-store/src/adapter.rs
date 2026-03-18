use crate::{KvEngine, KvError};
use async_trait::async_trait;
use rk_core::store::{KvStore, LeaseId, ListResult, WatchStream};
use rk_core::watch::WatchEvent;
use rk_core::{Error, Result};
use std::sync::Arc;
use std::time::Duration;
use stormforce_kv::proto::{
    CompactionRequest, DeleteRangeRequest, LeaseGrantRequest, LeaseKeepAliveRequest,
    LeaseRevokeRequest, PutRequest, RangeRequest,
};
use stormforce_kv::lease::LeaseManager;
use stormforce_kv::store::MvccStore;
use stormforce_kv::watch::WatchHub;
use tokio::sync::mpsc;

/// KvStore implementation backed by stormforce-kv's KvEngine.
pub struct StormforceStore {
    engine: Arc<KvEngine>,
}

impl StormforceStore {
    /// Create a new store from a stormforce-kv engine.
    pub fn new(engine: Arc<KvEngine>) -> Self {
        Self { engine }
    }

    /// Create a store with a fresh in-process engine (no Raft, for single-node or testing).
    pub fn open(data_dir: &std::path::Path) -> Result<Self> {
        let store = Arc::new(MvccStore::open(data_dir).map_err(kv_err)?);
        let watch_hub = Arc::new(WatchHub::new(4096));
        let lease_mgr = Arc::new(LeaseManager::new());
        let engine = Arc::new(KvEngine::new(
            store,
            watch_hub,
            lease_mgr,
            None, // no Raft for single-node
            1,    // cluster_id
            1,    // member_id
        ));
        KvEngine::spawn_lease_reaper(engine.clone());
        Ok(Self { engine })
    }

    /// Access the underlying KvEngine for direct operations.
    pub fn engine(&self) -> &Arc<KvEngine> {
        &self.engine
    }
}

fn kv_err(e: KvError) -> Error {
    Error::Store(e.to_string())
}

#[async_trait]
impl KvStore for StormforceStore {
    async fn get(&self, key: &str) -> Result<Option<(Vec<u8>, u64)>> {
        let req = RangeRequest {
            key: key.as_bytes().to_vec(),
            ..Default::default()
        };
        let resp = self.engine.range(&req).map_err(kv_err)?;
        Ok(resp.kvs.first().map(|kv| (kv.value.clone(), kv.mod_revision as u64)))
    }

    async fn put(&self, key: &str, value: &[u8], prev_revision: Option<u64>) -> Result<u64> {
        // If prev_revision is specified, do a CAS via transaction
        if let Some(expected_rev) = prev_revision {
            let req = stormforce_kv::proto::TxnRequest {
                compare: vec![stormforce_kv::proto::Compare {
                    result: stormforce_kv::proto::CompareResult::Equal as i32,
                    target: stormforce_kv::proto::CompareTarget::Mod as i32,
                    key: key.as_bytes().to_vec(),
                    mod_revision: expected_rev as i64,
                    ..Default::default()
                }],
                success: vec![stormforce_kv::proto::RequestOp {
                    request_put: Some(PutRequest {
                        key: key.as_bytes().to_vec(),
                        value: value.to_vec(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                failure: vec![],
            };
            let resp = self.engine.txn(&req).await.map_err(kv_err)?;
            if !resp.succeeded {
                return Err(Error::Conflict);
            }
            let rev = resp.header.map(|h| h.revision as u64).unwrap_or(0);
            Ok(rev)
        } else {
            let req = PutRequest {
                key: key.as_bytes().to_vec(),
                value: value.to_vec(),
                ..Default::default()
            };
            let resp = self.engine.put(&req).await.map_err(kv_err)?;
            let rev = resp.header.map(|h| h.revision as u64).unwrap_or(0);
            Ok(rev)
        }
    }

    async fn delete(&self, key: &str, prev_revision: Option<u64>) -> Result<()> {
        if let Some(expected_rev) = prev_revision {
            let req = stormforce_kv::proto::TxnRequest {
                compare: vec![stormforce_kv::proto::Compare {
                    result: stormforce_kv::proto::CompareResult::Equal as i32,
                    target: stormforce_kv::proto::CompareTarget::Mod as i32,
                    key: key.as_bytes().to_vec(),
                    mod_revision: expected_rev as i64,
                    ..Default::default()
                }],
                success: vec![stormforce_kv::proto::RequestOp {
                    request_delete_range: Some(DeleteRangeRequest {
                        key: key.as_bytes().to_vec(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                failure: vec![],
            };
            let resp = self.engine.txn(&req).await.map_err(kv_err)?;
            if !resp.succeeded {
                return Err(Error::Conflict);
            }
        } else {
            let req = DeleteRangeRequest {
                key: key.as_bytes().to_vec(),
                ..Default::default()
            };
            self.engine.delete_range(&req).await.map_err(kv_err)?;
        }
        Ok(())
    }

    async fn list(
        &self,
        prefix: &str,
        limit: usize,
        continue_token: Option<&str>,
    ) -> Result<ListResult> {
        // For prefix listing, stormforce-kv uses range_end = "\0" convention
        let start_key = if let Some(token) = continue_token {
            // continue_token is the last key seen + \0 to start after it
            let mut k = token.as_bytes().to_vec();
            k.push(0);
            k
        } else {
            prefix.as_bytes().to_vec()
        };

        let req = RangeRequest {
            key: start_key,
            range_end: prefix_range_end(prefix),
            limit: limit as i64,
            ..Default::default()
        };
        let resp = self.engine.range(&req).map_err(kv_err)?;
        let revision = resp.header.map(|h| h.revision as u64).unwrap_or(0);

        let items: Vec<(String, Vec<u8>, u64)> = resp
            .kvs
            .iter()
            .map(|kv| {
                (
                    String::from_utf8_lossy(&kv.key).to_string(),
                    kv.value.clone(),
                    kv.mod_revision as u64,
                )
            })
            .collect();

        let continue_token = if resp.more {
            items.last().map(|(k, _, _)| k.clone())
        } else {
            None
        };

        Ok(ListResult {
            items,
            continue_token,
            revision,
        })
    }

    async fn watch(&self, prefix: &str, start_revision: u64) -> Result<WatchStream> {
        let (tx, rx) = mpsc::channel(256);
        let watch_hub = self.engine.watch_hub.clone();
        let store = self.engine.store.clone();
        let key = prefix.as_bytes().to_vec();
        let range_end = prefix_range_end(prefix);

        tokio::spawn(async move {
            let mut watcher = watch_hub.watch(
                key,
                range_end,
                start_revision as i64,
                false,
            );

            // Replay historical events first
            if let Ok(events) = watcher.replay_and_watch(&store).await {
                for event in events {
                    let kv = event.kv.unwrap_or_default();
                    let watch_event = if event.r#type == 0 {
                        // Check if create_revision == mod_revision → Added, else Modified
                        if kv.create_revision == kv.mod_revision {
                            WatchEvent::Added {
                                key: String::from_utf8_lossy(&kv.key).to_string(),
                                value: kv.value.clone(),
                                revision: kv.mod_revision as u64,
                            }
                        } else {
                            WatchEvent::Modified {
                                key: String::from_utf8_lossy(&kv.key).to_string(),
                                value: kv.value.clone(),
                                revision: kv.mod_revision as u64,
                            }
                        }
                    } else {
                        WatchEvent::Deleted {
                            key: String::from_utf8_lossy(&kv.key).to_string(),
                            revision: kv.mod_revision as u64,
                        }
                    };
                    if tx.send(watch_event).await.is_err() {
                        return;
                    }
                }
            }

            // Then stream live events
            loop {
                match watcher.recv().await {
                    Some(event) => {
                        let watch_event = if event.event_type as i32 == 0 {
                            if event.kv.create_revision == event.kv.mod_revision {
                                WatchEvent::Added {
                                    key: String::from_utf8_lossy(&event.kv.key).to_string(),
                                    value: event.kv.value.clone(),
                                    revision: event.revision as u64,
                                }
                            } else {
                                WatchEvent::Modified {
                                    key: String::from_utf8_lossy(&event.kv.key).to_string(),
                                    value: event.kv.value.clone(),
                                    revision: event.revision as u64,
                                }
                            }
                        } else {
                            WatchEvent::Deleted {
                                key: String::from_utf8_lossy(&event.kv.key).to_string(),
                                revision: event.revision as u64,
                            }
                        };
                        if tx.send(watch_event).await.is_err() {
                            return;
                        }
                    }
                    None => return,
                }
            }
        });

        Ok(rx)
    }

    async fn lease_grant(&self, ttl: Duration) -> Result<LeaseId> {
        let req = LeaseGrantRequest {
            ttl: ttl.as_secs() as i64,
            id: 0, // auto-assign
        };
        let resp = self.engine.lease_grant(&req).await.map_err(kv_err)?;
        Ok(resp.id as u64)
    }

    async fn lease_keepalive(&self, id: LeaseId) -> Result<()> {
        let req = LeaseKeepAliveRequest { id: id as i64 };
        self.engine.lease_keep_alive(&req).map_err(kv_err)?;
        Ok(())
    }

    async fn lease_revoke(&self, id: LeaseId) -> Result<()> {
        let req = LeaseRevokeRequest { id: id as i64 };
        self.engine.lease_revoke(&req).await.map_err(kv_err)?;
        Ok(())
    }

    async fn compact(&self, revision: u64) -> Result<()> {
        let req = CompactionRequest {
            revision: revision as i64,
            physical: false,
        };
        self.engine.compact(&req).await.map_err(kv_err)?;
        Ok(())
    }
}

/// Compute range_end for prefix scanning.
/// For stormforce-kv, prefix scan uses range_end = prefix with last byte incremented.
fn prefix_range_end(prefix: &str) -> Vec<u8> {
    let mut end = prefix.as_bytes().to_vec();
    if end.is_empty() {
        // Empty prefix = scan everything
        return vec![0];
    }
    // Increment the last byte
    if let Some(last) = end.last_mut() {
        *last += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_store_crud() {
        let dir = tempfile::tempdir().unwrap();
        let store = StormforceStore::open(dir.path()).unwrap();

        // Put
        let rev = store.put("/test/key1", b"value1", None).await.unwrap();
        assert!(rev > 0);

        // Get
        let result = store.get("/test/key1").await.unwrap();
        assert!(result.is_some());
        let (val, _) = result.unwrap();
        assert_eq!(val, b"value1");

        // List
        let list = store.list("/test/", 100, None).await.unwrap();
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].0, "/test/key1");

        // Delete
        store.delete("/test/key1", None).await.unwrap();
        let result = store.get("/test/key1").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_store_cas() {
        let dir = tempfile::tempdir().unwrap();
        let store = StormforceStore::open(dir.path()).unwrap();

        let rev1 = store.put("/cas/key", b"v1", None).await.unwrap();

        // CAS with correct revision succeeds
        let rev2 = store.put("/cas/key", b"v2", Some(rev1)).await.unwrap();
        assert!(rev2 > rev1);

        // CAS with stale revision fails
        let result = store.put("/cas/key", b"v3", Some(rev1)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_store_lease() {
        let dir = tempfile::tempdir().unwrap();
        let store = StormforceStore::open(dir.path()).unwrap();

        let lease_id = store.lease_grant(Duration::from_secs(60)).await.unwrap();
        assert!(lease_id > 0);

        store.lease_keepalive(lease_id).await.unwrap();
        store.lease_revoke(lease_id).await.unwrap();
    }
}
