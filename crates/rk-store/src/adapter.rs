//! `EtcdStore` — a `KvStore` implementation over the etcd v3 gRPC wire protocol.
//!
//! Talks to an external datastore (fastetcd, or any etcd v3 server) via the
//! `etcd-client` crate. The `KvStore` trait maps almost 1:1 onto etcd RPCs:
//! `get`→Range, `put`→Put/Txn, `delete`→DeleteRange/Txn, `list`→Range,
//! `watch`→Watch, `lease_*`→Lease*, `compact`→Compact.
//!
//! `resourceVersion` == etcd `mod_revision`; optimistic concurrency is done
//! with a Txn comparing `mod_revision`, exactly as upstream kube does.

use async_trait::async_trait;
use etcd_client::EventType;
use etcd_client::{
    Certificate, Client, Compare, CompareOp, ConnectOptions, GetOptions, Identity, TlsOptions, Txn,
    TxnOp, WatchOptions,
};
use rk_core::store::{KvStore, LeaseId, ListResult, WatchStream};
use rk_core::watch::WatchEvent;
use rk_core::{Error, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

/// TLS material for connecting to etcd/fastetcd.
///
/// fastetcd v0.8.1+ enforces `--client-cert-auth`, so `cert`/`key` are required
/// against a hardened server; `ca` verifies the server certificate.
#[derive(Debug, Clone, Default)]
pub struct EtcdTls {
    /// CA certificate (PEM) used to verify the server.
    pub ca: Option<PathBuf>,
    /// Client certificate (PEM) for mutual TLS.
    pub cert: Option<PathBuf>,
    /// Client private key (PEM) for mutual TLS.
    pub key: Option<PathBuf>,
}

/// KvStore implementation backed by an external etcd v3 datastore (fastetcd).
///
/// `Client` is cheaply cloneable (it shares one gRPC channel), so each trait
/// method clones it to get a `&mut` handle without opening extra connections.
#[derive(Clone)]
pub struct EtcdStore {
    client: Client,
}

impl EtcdStore {
    /// Connect to one or more etcd/fastetcd endpoints (e.g. `https://127.0.0.1:2379`).
    ///
    /// Pass `tls = None` for a plaintext (`http://`) connection; pass `Some(_)`
    /// to enable TLS / mutual TLS against a hardened fastetcd.
    pub async fn connect(endpoints: &[String], tls: Option<EtcdTls>) -> Result<Self> {
        let mut opts = ConnectOptions::new();
        if let Some(tls) = tls {
            let mut t = TlsOptions::new();
            if let Some(ca) = &tls.ca {
                let pem = std::fs::read(ca).map_err(io_err)?;
                t = t.ca_certificate(Certificate::from_pem(pem));
            }
            if let (Some(cert), Some(key)) = (&tls.cert, &tls.key) {
                let cert_pem = std::fs::read(cert).map_err(io_err)?;
                let key_pem = std::fs::read(key).map_err(io_err)?;
                t = t.identity(Identity::from_pem(cert_pem, key_pem));
            }
            opts = opts.with_tls(t);
        }
        let client = Client::connect(endpoints, Some(opts))
            .await
            .map_err(etcd_err)?;
        Ok(Self { client })
    }

    /// Wrap an already-connected client (useful for sharing one connection).
    pub fn from_client(client: Client) -> Self {
        Self { client }
    }
}

fn etcd_err(e: etcd_client::Error) -> Error {
    Error::Store(e.to_string())
}

fn io_err(e: std::io::Error) -> Error {
    Error::Store(format!("reading TLS material: {e}"))
}

#[async_trait]
impl KvStore for EtcdStore {
    async fn get(&self, key: &str) -> Result<Option<(Vec<u8>, u64)>> {
        let mut client = self.client.clone();
        let resp = client.get(key, None).await.map_err(etcd_err)?;
        Ok(resp
            .kvs()
            .first()
            .map(|kv| (kv.value().to_vec(), kv.mod_revision() as u64)))
    }

    async fn put(&self, key: &str, value: &[u8], prev_revision: Option<u64>) -> Result<u64> {
        let mut client = self.client.clone();
        match prev_revision {
            // CAS: only write if the key's mod_revision matches. A brand-new key
            // has mod_revision 0, so Some(0) is create-if-not-exists, matching the
            // API server's create/update semantics.
            Some(expected_rev) => {
                let txn = Txn::new()
                    .when(vec![Compare::mod_revision(
                        key,
                        CompareOp::Equal,
                        expected_rev as i64,
                    )])
                    .and_then(vec![TxnOp::put(key, value.to_vec(), None)]);
                let resp = client.txn(txn).await.map_err(etcd_err)?;
                if !resp.succeeded() {
                    return Err(Error::Conflict);
                }
                Ok(resp.header().map(|h| h.revision() as u64).unwrap_or(0))
            }
            None => {
                let resp = client
                    .put(key, value.to_vec(), None)
                    .await
                    .map_err(etcd_err)?;
                Ok(resp.header().map(|h| h.revision() as u64).unwrap_or(0))
            }
        }
    }

    async fn delete(&self, key: &str, prev_revision: Option<u64>) -> Result<()> {
        let mut client = self.client.clone();
        match prev_revision {
            Some(expected_rev) => {
                let txn = Txn::new()
                    .when(vec![Compare::mod_revision(
                        key,
                        CompareOp::Equal,
                        expected_rev as i64,
                    )])
                    .and_then(vec![TxnOp::delete(key, None)]);
                let resp = client.txn(txn).await.map_err(etcd_err)?;
                if !resp.succeeded() {
                    return Err(Error::Conflict);
                }
            }
            None => {
                client.delete(key, None).await.map_err(etcd_err)?;
            }
        }
        Ok(())
    }

    async fn list(
        &self,
        prefix: &str,
        limit: usize,
        continue_token: Option<&str>,
    ) -> Result<ListResult> {
        let mut client = self.client.clone();

        // Prefix scan over [start_key, range_end). On continuation, resume just
        // after the last key seen (token + \0).
        let start_key = match continue_token {
            Some(token) => {
                let mut k = token.as_bytes().to_vec();
                k.push(0);
                k
            }
            None => prefix.as_bytes().to_vec(),
        };

        let opts = GetOptions::new()
            .with_range(prefix_range_end(prefix))
            .with_limit(limit as i64);
        let resp = client.get(start_key, Some(opts)).await.map_err(etcd_err)?;
        let revision = resp.header().map(|h| h.revision() as u64).unwrap_or(0);

        let items: Vec<(String, Vec<u8>, u64)> = resp
            .kvs()
            .iter()
            .map(|kv| {
                (
                    String::from_utf8_lossy(kv.key()).to_string(),
                    kv.value().to_vec(),
                    kv.mod_revision() as u64,
                )
            })
            .collect();

        let continue_token = if resp.more() {
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
        let mut client = self.client.clone();
        let (tx, rx) = mpsc::channel(256);

        let opts = WatchOptions::new()
            .with_range(prefix_range_end(prefix))
            .with_start_revision(start_revision as i64);
        let (watcher, mut stream) = client
            .watch(prefix.as_bytes().to_vec(), Some(opts))
            .await
            .map_err(etcd_err)?;

        tokio::spawn(async move {
            // Hold the watcher for the task's lifetime — dropping it cancels the
            // server-side watch. etcd replays from start_revision, so historical
            // and live events arrive on the same stream.
            let _watcher = watcher;
            loop {
                match stream.message().await {
                    Ok(Some(resp)) => {
                        for event in resp.events() {
                            let Some(kv) = event.kv() else { continue };
                            let key = String::from_utf8_lossy(kv.key()).to_string();
                            let watch_event = match event.event_type() {
                                EventType::Put => {
                                    // create_revision == mod_revision → first write → Added.
                                    if kv.create_revision() == kv.mod_revision() {
                                        WatchEvent::Added {
                                            key,
                                            value: kv.value().to_vec(),
                                            revision: kv.mod_revision() as u64,
                                        }
                                    } else {
                                        WatchEvent::Modified {
                                            key,
                                            value: kv.value().to_vec(),
                                            revision: kv.mod_revision() as u64,
                                        }
                                    }
                                }
                                EventType::Delete => WatchEvent::Deleted {
                                    key,
                                    revision: kv.mod_revision() as u64,
                                },
                            };
                            if tx.send(watch_event).await.is_err() {
                                return;
                            }
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        tracing::warn!("etcd watch stream error: {e}");
                        return;
                    }
                }
            }
        });

        Ok(rx)
    }

    async fn lease_grant(&self, ttl: Duration) -> Result<LeaseId> {
        let mut client = self.client.clone();
        let resp = client
            .lease_grant(ttl.as_secs() as i64, None)
            .await
            .map_err(etcd_err)?;
        Ok(resp.id() as u64)
    }

    async fn lease_keepalive(&self, id: LeaseId) -> Result<()> {
        let mut client = self.client.clone();
        // Open a keepalive channel and send a single heartbeat.
        let (mut keeper, _stream) = client.lease_keep_alive(id as i64).await.map_err(etcd_err)?;
        keeper.keep_alive().await.map_err(etcd_err)?;
        Ok(())
    }

    async fn lease_revoke(&self, id: LeaseId) -> Result<()> {
        let mut client = self.client.clone();
        client.lease_revoke(id as i64).await.map_err(etcd_err)?;
        Ok(())
    }

    async fn compact(&self, revision: u64) -> Result<()> {
        let mut client = self.client.clone();
        client
            .compact(revision as i64, None)
            .await
            .map_err(etcd_err)?;
        Ok(())
    }
}

/// Compute the exclusive `range_end` for a prefix scan: the prefix with its
/// last byte incremented (etcd's standard prefix-range convention). An empty
/// prefix scans the whole keyspace.
fn prefix_range_end(prefix: &str) -> Vec<u8> {
    let mut end = prefix.as_bytes().to_vec();
    if end.is_empty() {
        return vec![0];
    }
    if let Some(last) = end.last_mut() {
        *last += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Endpoint for integration tests. These require a running etcd/fastetcd
    /// (e.g. `docker run -p 2379:2379 fastetcd`) and are ignored by default.
    fn test_endpoints() -> Vec<String> {
        vec![std::env::var("RK_TEST_ETCD").unwrap_or_else(|_| "http://127.0.0.1:2379".to_string())]
    }

    #[tokio::test]
    #[ignore = "requires a running etcd/fastetcd on :2379"]
    async fn test_store_crud() {
        let store = EtcdStore::connect(&test_endpoints(), None).await.unwrap();

        let rev = store.put("/test/key1", b"value1", None).await.unwrap();
        assert!(rev > 0);

        let (val, _) = store.get("/test/key1").await.unwrap().unwrap();
        assert_eq!(val, b"value1");

        let list = store.list("/test/", 100, None).await.unwrap();
        assert!(list.items.iter().any(|(k, _, _)| k == "/test/key1"));

        store.delete("/test/key1", None).await.unwrap();
        assert!(store.get("/test/key1").await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "requires a running etcd/fastetcd on :2379"]
    async fn test_store_cas() {
        let store = EtcdStore::connect(&test_endpoints(), None).await.unwrap();

        let rev1 = store.put("/cas/key", b"v1", None).await.unwrap();
        let rev2 = store.put("/cas/key", b"v2", Some(rev1)).await.unwrap();
        assert!(rev2 > rev1);

        // Stale revision must conflict.
        assert!(store.put("/cas/key", b"v3", Some(rev1)).await.is_err());
        store.delete("/cas/key", None).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a running etcd/fastetcd on :2379"]
    async fn test_store_lease() {
        let store = EtcdStore::connect(&test_endpoints(), None).await.unwrap();

        let lease_id = store.lease_grant(Duration::from_secs(60)).await.unwrap();
        assert!(lease_id > 0);
        store.lease_keepalive(lease_id).await.unwrap();
        store.lease_revoke(lease_id).await.unwrap();
    }
}
