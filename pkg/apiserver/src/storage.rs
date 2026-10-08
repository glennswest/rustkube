//! Generic resource storage layer.
//!
//! Bridges K8s resource CRUD to the KvStore, handling JSON serialization,
//! resourceVersion tracking, key construction, and metadata injection.

use crate::error::ApiError;
use crate::watch_cache::WatchCache;
use apimachinery::store::KvStore;
use serde_json::Value;
use std::sync::Arc;

/// Key prefix for all resources in the store.
const REGISTRY_PREFIX: &str = "/registry";

/// The resource a store key or prefix belongs to, for metric labels.
///
/// Built-ins are `/registry/{resource}/...`, so it is the second segment.
/// Custom resources are `/registry/{group}/{plural}/...` (#76), recognisable
/// by the dot every CRD group has, and are labelled `{plural}.{group}` — the
/// CRD's own name, which is how upstream labels them. An unrecognisable key
/// is labelled `unknown` rather than dropped, because a slow call is worth
/// seeing even when we cannot say what it was for.
pub(crate) fn metric_resource(key: &str) -> String {
    let mut segs = key.trim_start_matches('/').split('/').skip(1);
    match segs.next().filter(|s| !s.is_empty()) {
        Some(group) if group.contains('.') => match segs.next().filter(|s| !s.is_empty()) {
            Some(plural) => format!("{plural}.{group}"),
            None => group.to_string(),
        },
        Some(resource) => resource.to_string(),
        None => "unknown".to_string(),
    }
}

/// Generic resource storage backed by the shared datastore.
pub struct ResourceStorage {
    store: Arc<dyn KvStore>,
    watch_cache: Arc<WatchCache>,
}

impl ResourceStorage {
    pub fn new(store: Arc<dyn KvStore>) -> Self {
        let watch_cache = Arc::new(WatchCache::new(store.clone()));
        Self { store, watch_cache }
    }

    /// The shared watch cache, for readers that keep their own view of a
    /// prefix (the RBAC authorizer, #177).
    pub(crate) fn watch_cache(&self) -> &Arc<WatchCache> {
        &self.watch_cache
    }

    /// The resource name inside a store key or prefix, for the `type` label on
    /// `etcd_request_duration_seconds`.
    fn resource_of(key: &str) -> String {
        metric_resource(key)
    }

    /// The key segment a custom resource is stored under: `{group}/{plural}`.
    ///
    /// Plurals are unique only *within* a group, so a plural alone is not a
    /// keyspace. Keyed by plural, `baremetalhosts.metal3.io` and
    /// `baremetalhosts.metal.storm.io` read and wrote the same keys: a list of
    /// one returned the other's objects under the wrong kind, and the second
    /// object of a name was refused as AlreadyExists (#76). This is upstream's
    /// layout, `/registry/{group}/{plural}/...`.
    ///
    /// It cannot shadow a built-in either: a CRD group always contains a dot
    /// and a built-in plural never does, so `/registry/{group}/` and
    /// `/registry/{plural}/` are disjoint. Pass the result anywhere a resource
    /// name goes — `cluster_key`, `namespaced_key` and the prefixes.
    pub fn custom_resource(group: &str, plural: &str) -> String {
        format!("{group}/{plural}")
    }

    /// Build the store key for a cluster-scoped resource.
    pub fn cluster_key(resource: &str, name: &str) -> String {
        format!("{REGISTRY_PREFIX}/{resource}/{name}")
    }

    /// Build the store key for a namespace-scoped resource.
    pub fn namespaced_key(resource: &str, namespace: &str, name: &str) -> String {
        format!("{REGISTRY_PREFIX}/{resource}/{namespace}/{name}")
    }

    /// Prefix for listing all instances of a cluster-scoped resource.
    pub fn cluster_prefix(resource: &str) -> String {
        format!("{REGISTRY_PREFIX}/{resource}/")
    }

    /// Prefix for listing all instances of a namespaced resource in one namespace.
    pub fn namespace_prefix(resource: &str, namespace: &str) -> String {
        format!("{REGISTRY_PREFIX}/{resource}/{namespace}/")
    }

    /// Prefix for listing all instances of a namespaced resource across all namespaces.
    pub fn all_namespaces_prefix(resource: &str) -> String {
        format!("{REGISTRY_PREFIX}/{resource}/")
    }

    /// Get a single resource by key.
    pub async fn get(&self, key: &str) -> Result<Value, ApiError> {
        let _timer = apimachinery::metrics::StoreTimer::new("get", Self::resource_of(key));
        match self.store.get(key).await.map_err(ApiError::from)? {
            Some((bytes, rev)) => {
                let mut obj: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| ApiError::internal(&e.to_string()))?;
                // resourceVersion is the store's mod_revision, NOT whatever was
                // baked into the JSON on the last write (#33). Returning a stale
                // baked-in value breaks optimistic concurrency: the client PUTs
                // it back, the store CASes it against the real mod_revision, and
                // the mismatch 409s — which loops leader election forever.
                inject_resource_version(&mut obj, rev);
                Ok(obj)
            }
            None => Err(not_found_at(key)),
        }
    }

    /// List resources by prefix with pagination.
    pub async fn list(
        &self,
        prefix: &str,
        limit: usize,
        continue_token: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>, u64), ApiError> {
        let page = self.list_page(prefix, limit, continue_token).await?;
        Ok((page.items, page.continue_token, page.revision))
    }

    /// A page of a LIST, with what a list's metadata needs.
    ///
    /// Each item carries its own `resourceVersion` — the revision that last
    /// wrote it, which a client compares with what a later write returns
    /// (#111). The continue token is `{revision}:{last key}`: every page of
    /// one paged LIST reads the same MVCC snapshot, including when pages hit
    /// different API servers. Compacted snapshots return 410. The first page
    /// is a linearizable datastore read, independent of a replica's cache.
    /// A legacy bare-key token starts from the current snapshot.
    pub async fn list_page(
        &self,
        prefix: &str,
        limit: usize,
        continue_token: Option<&str>,
    ) -> Result<ListPage, ApiError> {
        let (pinned, continue_token) = match continue_token.map(parse_continue) {
            Some((rev, key)) => (rev, Some(key)),
            None => (None, None),
        };
        self.list_pinned(prefix, limit, continue_token, pinned).await
    }

    /// A page of `prefix` from the store after `continue_key`, at `pinned`
    /// (or the current revision).
    async fn list_pinned(
        &self,
        prefix: &str,
        limit: usize,
        continue_token: Option<&str>,
        pinned: Option<u64>,
    ) -> Result<ListPage, ApiError> {
        let _timer = apimachinery::metrics::StoreTimer::new("list", Self::resource_of(prefix));
        // An API server's local recent-write table cannot establish freshness
        // for writes accepted by another master. Read the shared datastore,
        // pinning continuation pages to the first page's actual revision.
        let page = match self.store.list_at(prefix, limit, continue_token, pinned).await {
            Ok(page) => page,
            // The token's snapshot was compacted away (#139): upstream's 410
            // with an "inconsistent" token that lists the rest from the same
            // key at the current revision — a bare key, which starts from the
            // current snapshot and pins the pages after it to that one.
            Err(apimachinery::Error::Gone(_)) if continue_token.is_some() => {
                let mut gone = ApiError::gone(INCONSISTENT_CONTINUE);
                gone.continue_token = continue_token.map(str::to_string);
                return Err(gone);
            }
            Err(e) => return Err(e.into()),
        };

        let mut items = Vec::with_capacity(page.items.len());
        for (_, bytes, mod_rev) in &page.items {
            let mut obj: Value =
                serde_json::from_slice(bytes).map_err(|e| ApiError::internal(&e.to_string()))?;
            inject_resource_version(&mut obj, *mod_rev);
            items.push(obj);
        }
        let revision = pinned.unwrap_or(page.revision);
        Ok(ListPage {
            items,
            continue_token: page.continue_token.map(|k| format!("{revision}:{k}")),
            revision,
            remaining: page.remaining,
        })
    }

    /// GET `key` as `read` asks (#171): a consistent read from the store, or
    /// the object from the watch cache of `cache_prefix` (the resource-wide
    /// prefix holding `key`) once it reflects the requested revision.
    pub async fn get_read(&self, cache_prefix: &str, key: &str, read: Read) -> Result<Value, ApiError> {
        let Read::NotOlderThan(min) = read else {
            return self.get(key).await;
        };
        match self.watch_cache.read_one(cache_prefix, key, min).await {
            Ok(Some((Some((bytes, mod_rev)), _))) => {
                apimachinery::metrics::record_cache_read("get", &Self::resource_of(key));
                let mut obj: Value =
                    serde_json::from_slice(&bytes).map_err(|e| ApiError::internal(&e.to_string()))?;
                inject_resource_version(&mut obj, mod_rev);
                Ok(obj)
            }
            Ok(Some((None, _))) => {
                apimachinery::metrics::record_cache_read("get", &Self::resource_of(key));
                Err(not_found_at(key))
            }
            // Not caught up, or no cache: the store, which is never older.
            _ => self.get(key).await,
        }
    }

    /// A LIST page of `prefix` as `read` asks (#171), with upstream's
    /// cacher rules:
    ///
    /// - a continuation page is the store, pinned to the revision in its
    ///   token, whatever else the request says;
    /// - `Consistent` (no `resourceVersion`) is the store at its current
    ///   revision; `Exact(n)` the store pinned to `n`;
    /// - `NotOlderThan(n)` (`resourceVersion=0`, or `n` without `Exact`) is the
    ///   watch cache of `cache_prefix` once it reflects `n`, else the store.
    ///
    /// A page from the cache hands out the same `{revision}:{key}` token, so
    /// the next page is the store pinned to the cache's revision — the same
    /// snapshot. A relist storm after a restart (every informer asks for
    /// `resourceVersion=0`) is then one seed per resource, not one store
    /// Range per client (#5).
    pub async fn list_page_read(
        &self,
        cache_prefix: &str,
        prefix: &str,
        limit: usize,
        continue_token: Option<&str>,
        read: Read,
    ) -> Result<ListPage, ApiError> {
        match (continue_token, read) {
            (Some(_), _) | (None, Read::Consistent) => self.list_page(prefix, limit, continue_token).await,
            (None, Read::Exact(rev)) => self.list_page_at(prefix, limit, rev).await,
            (None, Read::NotOlderThan(min)) => {
                match self.watch_cache.read_page(cache_prefix, prefix, limit, None, min).await {
                    Ok(Some(page)) => {
                        apimachinery::metrics::record_cache_read("list", &Self::resource_of(prefix));
                        let mut items = Vec::with_capacity(page.items.len());
                        for (bytes, mod_rev) in &page.items {
                            let mut obj: Value = serde_json::from_slice(bytes)
                                .map_err(|e| ApiError::internal(&e.to_string()))?;
                            inject_resource_version(&mut obj, *mod_rev);
                            items.push(obj);
                        }
                        Ok(ListPage {
                            items,
                            continue_token: page.continue_key.map(|k| format!("{}:{k}", page.revision)),
                            revision: page.revision,
                            remaining: page.remaining,
                        })
                    }
                    _ => self.list_page(prefix, limit, None).await,
                }
            }
        }
    }

    /// The first page of `prefix` at exactly revision `rev`
    /// (`resourceVersionMatch=Exact`): 410 once compacted.
    async fn list_page_at(&self, prefix: &str, limit: usize, rev: u64) -> Result<ListPage, ApiError> {
        self.list_pinned(prefix, limit, None, Some(rev)).await
    }

    /// Create a resource (fails if it already exists).
    pub async fn create(&self, key: &str, mut obj: Value) -> Result<Value, ApiError> {
        let _timer = apimachinery::metrics::StoreTimer::new("create", Self::resource_of(key));
        // Never persist resourceVersion in the stored bytes — it is derived from
        // the store's mod_revision on read (#33). Baking it in makes later reads
        // return a stale RV.
        strip_resource_version(&mut obj);
        let bytes = serde_json::to_vec(&obj).map_err(|e| ApiError::internal(&e.to_string()))?;
        // Atomic create-if-not-exists: CAS against revision 0 (the store treats
        // an absent key as revision 0) fails with Conflict if the key exists, so
        // two concurrent creates can't both win.
        let rev = match self.store.put(key, &bytes, Some(0)).await {
            Ok(rev) => rev,
            Err(apimachinery::Error::Conflict) => {
                let name = obj["metadata"]["name"].as_str().unwrap_or("unknown");
                let kind = obj["kind"].as_str().unwrap_or("resource");
                return Err(ApiError::already_exists(kind, name));
            }
            Err(e) => return Err(ApiError::from(e)),
        };

        inject_resource_version(&mut obj, rev);
        Ok(obj)
    }

    /// Update a resource (requires resourceVersion for optimistic concurrency).
    pub async fn update(
        &self,
        key: &str,
        mut obj: Value,
        prev_revision: Option<u64>,
    ) -> Result<Value, ApiError> {
        let _timer = apimachinery::metrics::StoreTimer::new("update", Self::resource_of(key));
        // Strip the client-supplied resourceVersion from the stored bytes: it is
        // used for the CAS (prev_revision) but must not be baked into storage, or
        // the next read returns a stale RV and optimistic concurrency breaks (#33).
        strip_resource_version(&mut obj);
        let bytes = serde_json::to_vec(&obj).map_err(|e| ApiError::internal(&e.to_string()))?;
        let rev = self
            .store
            .put(key, &bytes, prev_revision)
            .await
            .map_err(ApiError::from)?;

        inject_resource_version(&mut obj, rev);
        Ok(obj)
    }

    /// Delete a resource by key.
    pub async fn delete(&self, key: &str, prev_revision: Option<u64>) -> Result<(), ApiError> {
        let _timer = apimachinery::metrics::StoreTimer::new("delete", Self::resource_of(key));
        self.store
            .delete(key, prev_revision)
            .await
            .map_err(ApiError::from)?;
        Ok(())
    }

    /// Watch resources by prefix, served through the shared watch cache (one
    /// upstream store watch per prefix, fanned out in-memory).
    pub async fn watch(
        &self,
        prefix: &str,
        start_revision: u64,
    ) -> Result<apimachinery::store::WatchStream, ApiError> {
        self.watch_cache
            .watch(prefix, start_revision)
            .await
            .map_err(ApiError::from)
    }
}

/// Upstream's message for a continue token whose snapshot is compacted.
const INCONSISTENT_CONTINUE: &str = "The provided continue parameter is too old to display a consistent list result. You can start a new list without the continue parameter, or use the continue token in this response to retrieve the remainder of the results. Continuing with the provided token results in an inconsistent list - objects that were created, modified, or deleted between the time the first chunk was returned and now may show up in the list.";

/// A continue token: `{revision}:{key}`, or a bare key (keys start with `/`).
fn parse_continue(token: &str) -> (Option<u64>, &str) {
    match token.split_once(':') {
        Some((rev, key)) if !rev.is_empty() && rev.bytes().all(|b| b.is_ascii_digit()) => {
            (rev.parse().ok(), key)
        }
        _ => (None, token),
    }
}

#[cfg(test)]
mod read_tests {
    use super::Read;

    #[test]
    fn read_follows_resource_version_and_match() {
        let r = |rv: Option<&str>, m: Option<&str>| Read::from_params(rv, m);
        assert_eq!(r(None, None).unwrap(), Read::Consistent);
        assert_eq!(r(Some(""), None).unwrap(), Read::Consistent);
        assert_eq!(r(Some("0"), None).unwrap(), Read::NotOlderThan(0));
        assert_eq!(r(Some("42"), None).unwrap(), Read::NotOlderThan(42));
        assert_eq!(r(Some("42"), Some("NotOlderThan")).unwrap(), Read::NotOlderThan(42));
        assert_eq!(r(Some("42"), Some("Exact")).unwrap(), Read::Exact(42));
        assert!(r(Some("0"), Some("Exact")).is_err());
        assert!(r(Some("x"), None).is_err());
        assert!(r(None, Some("Exact")).is_err());
        assert!(r(Some("1"), Some("Newest")).is_err());
    }
}

#[cfg(test)]
mod continue_tests {
    #[test]
    fn a_continue_token_pins_the_revision() {
        assert_eq!(
            super::parse_continue("42:/registry/pods/a/p1"),
            (Some(42), "/registry/pods/a/p1")
        );
        assert_eq!(
            super::parse_continue("/registry/pods/a/p:1"),
            (None, "/registry/pods/a/p:1")
        );
        assert_eq!(super::parse_continue(":/x"), (None, ":/x"));
    }
}

/// A page of a LIST: see `ResourceStorage::list_page`.
/// Where a read may be served from, by its `resourceVersion` and
/// `resourceVersionMatch` (#171) — upstream's semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// No `resourceVersion`: the most recent state, from the store.
    Consistent,
    /// `resourceVersion=n` (`0` = any): at least that fresh, from the watch
    /// cache when it is.
    NotOlderThan(u64),
    /// `resourceVersionMatch=Exact`: the state at exactly that revision.
    Exact(u64),
}

impl Read {
    /// From the request's `resourceVersion` and `resourceVersionMatch`. A
    /// `resourceVersion` that is not a number, or a match without one, is a
    /// 400, as upstream answers.
    pub fn from_params(resource_version: Option<&str>, matching: Option<&str>) -> Result<Self, ApiError> {
        let rv = match resource_version.filter(|v| !v.is_empty()) {
            None if matching.is_some_and(|m| !m.is_empty()) => {
                return Err(ApiError::bad_request(
                    "resourceVersionMatch is forbidden unless resourceVersion is provided",
                ))
            }
            None => return Ok(Read::Consistent),
            Some(v) => v.parse::<u64>().map_err(|_| {
                ApiError::bad_request(&format!("invalid resourceVersion {v:?}: must be a number"))
            })?,
        };
        match matching.unwrap_or("") {
            "" | "NotOlderThan" => Ok(Read::NotOlderThan(rv)),
            "Exact" if rv == 0 => Err(ApiError::bad_request(
                "resourceVersionMatch \"Exact\" is forbidden for resourceVersion \"0\"",
            )),
            "Exact" => Ok(Read::Exact(rv)),
            m => Err(ApiError::bad_request(&format!(
                "invalid resourceVersionMatch {m:?}: must be NotOlderThan or Exact"
            ))),
        }
    }
}

pub struct ListPage {
    pub items: Vec<Value>,
    pub continue_token: Option<String>,
    pub revision: u64,
    /// Objects after this page (`metadata.remainingItemCount`), when known.
    pub remaining: Option<u64>,
}

/// Set `metadata.resourceVersion` to the store revision the object reflects.
fn inject_resource_version(obj: &mut Value, rev: u64) {
    if !obj.get("metadata").map(Value::is_object).unwrap_or(false) {
        obj["metadata"] = serde_json::json!({});
    }
    obj["metadata"]["resourceVersion"] = Value::String(rev.to_string());
}

/// Remove `metadata.resourceVersion` so it is never persisted in the stored
/// bytes (it is always derived from the store's mod_revision on read).
fn strip_resource_version(obj: &mut Value) {
    if let Some(meta) = obj.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        meta.remove("resourceVersion");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_overwrites_stale_resource_version() {
        // A read must report the store revision, not a stale baked-in RV (#33).
        let mut obj = serde_json::json!({
            "metadata": { "name": "x", "resourceVersion": "100" }
        });
        inject_resource_version(&mut obj, 237_900);
        assert_eq!(obj["metadata"]["resourceVersion"], "237900");
    }

    #[test]
    fn strip_removes_resource_version_for_storage() {
        let mut obj = serde_json::json!({
            "metadata": { "name": "x", "resourceVersion": "237871", "uid": "u" }
        });
        strip_resource_version(&mut obj);
        assert!(obj["metadata"].get("resourceVersion").is_none());
        // Other metadata is preserved.
        assert_eq!(obj["metadata"]["uid"], "u");
    }

    #[test]
    fn inject_tolerates_missing_metadata() {
        let mut obj = serde_json::json!({ "kind": "Lease" });
        inject_resource_version(&mut obj, 5);
        assert_eq!(obj["metadata"]["resourceVersion"], "5");
    }

    /// Custom resources are keyed by group, and labelled by CRD name (#76).
    #[test]
    fn custom_resource_keys_carry_the_group() {
        let a = ResourceStorage::custom_resource("metal3.io", "baremetalhosts");
        let b = ResourceStorage::custom_resource("metal.storm.io", "baremetalhosts");
        assert_eq!(
            ResourceStorage::namespaced_key(&a, "ns", "h1"),
            "/registry/metal3.io/baremetalhosts/ns/h1"
        );
        // Same plural, different group: disjoint prefixes.
        let (pa, pb) = (
            ResourceStorage::cluster_prefix(&a),
            ResourceStorage::cluster_prefix(&b),
        );
        assert!(!pa.starts_with(&pb) && !pb.starts_with(&pa));

        assert_eq!(
            metric_resource("/registry/metal3.io/baremetalhosts/ns/h1"),
            "baremetalhosts.metal3.io"
        );
        assert_eq!(
            metric_resource("/registry/metal3.io/baremetalhosts/"),
            "baremetalhosts.metal3.io"
        );
        assert_eq!(metric_resource("/registry/pods/default/p"), "pods");
        assert_eq!(metric_resource("/registry/"), "unknown");
    }
}

#[cfg(test)]
mod compacted_continue_tests {
    use super::ResourceStorage;
    use apimachinery::store::{KvStore, LeaseId, ListResult, WatchStream};
    use apimachinery::{Error, Result};
    use async_trait::async_trait;
    use axum::response::IntoResponse;
    use std::sync::Arc;
    use std::time::Duration;

    /// Three pods; history before revision 10 is compacted away.
    struct Compacted;

    #[async_trait]
    impl KvStore for Compacted {
        async fn get(&self, _: &str) -> Result<Option<(Vec<u8>, u64)>> {
            Ok(None)
        }
        async fn put(&self, _: &str, _: &[u8], _: Option<u64>) -> Result<u64> {
            unimplemented!()
        }
        async fn delete(&self, _: &str, _: Option<u64>) -> Result<u64> {
            unimplemented!()
        }
        async fn list(&self, p: &str, l: usize, c: Option<&str>) -> Result<ListResult> {
            self.list_at(p, l, c, None).await
        }
        async fn list_at(&self, _: &str, limit: usize, after: Option<&str>, pinned: Option<u64>) -> Result<ListResult> {
            if let Some(rev) = pinned.filter(|r| *r < 10) {
                return Err(Error::Gone(rev));
            }
            let keys = ["/registry/pods/a/p1", "/registry/pods/a/p2", "/registry/pods/a/p3"];
            let rest: Vec<_> = keys.iter().filter(|k| after.map_or(true, |a| **k > a)).collect();
            let page: Vec<_> = rest.iter().take(limit).map(|k| (k.to_string(), br#"{"metadata":{}}"#.to_vec(), 5)).collect();
            Ok(ListResult {
                continue_token: (rest.len() > limit).then(|| page.last().unwrap().0.clone()),
                items: page,
                revision: 12,
                remaining: None,
            })
        }
        async fn watch(&self, _: &str, _: u64) -> Result<WatchStream> {
            unimplemented!()
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
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_compacted_continue_is_410_expired_with_a_token_for_the_rest() {
        let storage = ResourceStorage::new(Arc::new(Compacted));
        let err = match storage.list_page("/registry/pods/", 1, Some("7:/registry/pods/a/p1")).await {
            Err(e) => e,
            Ok(_) => panic!("a token pinned below the compaction must not list"),
        };
        assert_eq!(err.status, axum::http::StatusCode::GONE);
        assert_eq!(err.reason, "Expired", "client-go's IsResourceExpired");
        let token = err.continue_token.clone().expect("an inconsistent continue token");

        let body = axum::body::to_bytes(err.into_response().into_body(), usize::MAX).await.unwrap();
        let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["code"], 410);
        assert_eq!(status["metadata"]["continue"], token.as_str());
        assert!(status["message"].as_str().unwrap().contains("inconsistent list"));

        // The token lists the rest from the same key, at the current
        // revision, and pins the pages after it there.
        let page = storage.list_page("/registry/pods/", 1, Some(&token)).await.ok().unwrap();
        assert_eq!(page.revision, 12);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.continue_token.as_deref(), Some("12:/registry/pods/a/p2"));
        let last = storage.list_page("/registry/pods/", 1, page.continue_token.as_deref()).await.ok().unwrap();
        assert_eq!((last.revision, last.items.len(), last.continue_token), (12, 1, None));

        // A token still inside history pages as before.
        assert!(storage.list_page("/registry/pods/", 1, Some("11:/registry/pods/a/p1")).await.is_ok());
    }
}

/// The NotFound for a missing object, named as the API names it (#109):
/// `namespaces "x" not found`, `deployments.apps "web" not found`,
/// `widgets.example.com "w" not found` — never the storage key, which used to
/// be the message (`resource "/registry/namespaces/x" not found`). A key is
/// `/registry/<plural>/[<ns>/]<name>`, or a custom resource's
/// `/registry/<group>/<plural>/[<ns>/]<name>` (a group has a dot, a plural
/// none, #76).
pub(crate) fn not_found_at(key: &str) -> ApiError {
    let segs: Vec<&str> = key.trim_start_matches("/registry/").split('/').filter(|s| !s.is_empty()).collect();
    let name = segs.last().copied().unwrap_or(key);
    let (group, plural) = match segs.as_slice() {
        [g, p, _, ..] if g.contains('.') => (g.to_string(), p.to_string()),
        [p, _, ..] => {
            let av = crate::handlers::resource::resource_to_api_version(p);
            (av.split_once('/').map(|(g, _)| g.to_string()).unwrap_or_default(), p.to_string())
        }
        _ => (String::new(), "resource".to_string()),
    };
    let resource = if group.is_empty() { plural } else { format!("{plural}.{group}") };
    ApiError::not_found(&resource, name)
}

#[cfg(test)]
mod not_found_tests {
    use super::not_found_at;

    #[test]
    fn a_missing_object_is_named_as_the_api_names_it() {
        let m = |k: &str| { let e = not_found_at(k); (e.message, e.details.unwrap()) };
        let (msg, d) = m("/registry/namespaces/openshift-config-managed");
        assert_eq!(msg, "namespaces \"openshift-config-managed\" not found");
        assert_eq!(d, serde_json::json!({"name": "openshift-config-managed", "group": "", "kind": "namespaces"}));
        assert_eq!(m("/registry/pods/default/web").0, "pods \"web\" not found");
        let (msg, d) = m("/registry/deployments/default/web");
        assert_eq!(msg, "deployments.apps \"web\" not found");
        assert_eq!(d["group"], "apps");
        assert_eq!(m("/registry/example.com/widgets/default/w").0, "widgets.example.com \"w\" not found");
        assert_eq!(m("/registry/example.com/clusterwidgets/w").0, "clusterwidgets.example.com \"w\" not found");
        assert!(!m("/registry/secrets/ns/s").0.contains("/registry"));
    }
}
