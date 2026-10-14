//! An in-memory `KvStore` for handler tests.
//!
//! Enough of etcd's semantics for a handler to be exercised as it runs:
//! one revision counter for the whole store, a key's revision is the one that
//! last wrote it, and a `prev_revision` is a compare-and-swap that fails with
//! `Conflict` when the key has moved on. Listing, watching and leases are not
//! here; a test that needs them needs fastetcd.

use apimachinery::store::{KvStore, LeaseId, ListResult, WatchStream};
use apimachinery::{Error, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// The store's revision, and key → (value, the revision that last wrote it).
type Data = (u64, BTreeMap<String, (Vec<u8>, u64)>);

#[derive(Default)]
pub struct MemStore {
    inner: Mutex<Data>,
    /// Every read fails as a datastore without quorum does.
    unavailable: AtomicBool,
}

impl MemStore {
    pub fn set_unavailable(&self, on: bool) {
        self.unavailable.store(on, Ordering::SeqCst);
    }
}

#[async_trait]
impl KvStore for MemStore {
    async fn get(&self, key: &str) -> Result<Option<(Vec<u8>, u64)>> {
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(Error::Unavailable("no quorum".into()));
        }
        Ok(self.inner.lock().unwrap().1.get(key).cloned())
    }

    async fn put(&self, key: &str, value: &[u8], prev_revision: Option<u64>) -> Result<u64> {
        let mut g = self.inner.lock().unwrap();
        if let Some(want) = prev_revision {
            let have = g.1.get(key).map(|(_, r)| *r).unwrap_or(0);
            if have != want {
                return Err(Error::Conflict);
            }
        }
        g.0 += 1;
        let rev = g.0;
        g.1.insert(key.to_string(), (value.to_vec(), rev));
        Ok(rev)
    }

    async fn delete(&self, key: &str, prev_revision: Option<u64>) -> Result<u64> {
        let mut g = self.inner.lock().unwrap();
        if let Some(want) = prev_revision {
            if g.1.get(key).map(|(_, r)| *r) != Some(want) {
                return Err(Error::Conflict);
            }
        }
        g.0 += 1;
        g.1.remove(key);
        Ok(g.0)
    }

    async fn list(&self, _: &str, _: usize, _: Option<&str>) -> Result<ListResult> {
        Err(Error::Store("MemStore does not list".into()))
    }
    async fn watch(&self, _: &str, _: u64) -> Result<WatchStream> {
        Err(Error::Store("MemStore does not watch".into()))
    }
    async fn lease_grant(&self, _: Duration) -> Result<LeaseId> {
        unimplemented!("MemStore has no leases")
    }
    async fn lease_keepalive(&self, _: LeaseId) -> Result<()> {
        unimplemented!("MemStore has no leases")
    }
    async fn lease_revoke(&self, _: LeaseId) -> Result<()> {
        unimplemented!("MemStore has no leases")
    }
    async fn compact(&self, _: u64) -> Result<()> {
        Ok(())
    }
}
