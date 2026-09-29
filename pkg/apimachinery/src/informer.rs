//! Indexed, UID-aware informer state. Callbacks run after publishing a snapshot.
//! Revisions remain opaque; acknowledgements match exact watch revisions.
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Key {
    pub namespace: String,
    pub name: String,
    pub uid: String,
}
impl Key {
    pub fn of(object: &Value) -> anyhow::Result<Self> {
        let metadata = &object["metadata"];
        let name = metadata["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("object has no name"))?;
        let uid = metadata["uid"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("object has no UID"))?;
        Ok(Self {
            namespace: metadata["namespace"].as_str().unwrap_or("").into(),
            name: name.into(),
            uid: uid.into(),
        })
    }
    fn name_key(&self) -> (String, String) {
        (self.namespace.clone(), self.name.clone())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Index {
    Namespace(String),
    Name(String, String),
    Owner(String),
    Node(String),
    Claim(String, String),
    StorageClass(String),
    Volume(String),
    Driver(String),
    Pod(String, String),
    Target(String, String, String),
    Reference(String, String, String),
    Label(String, String),
    /// Equality anchor for a selector; namespace prevents cross-namespace wakes.
    Selector(String, String, String),
    SelectorFallback(String),
}

fn indexes(object: &Value, key: &Key) -> HashSet<Index> {
    let mut result = HashSet::from([Index::Namespace(key.namespace.clone()), Index::Name(key.namespace.clone(), key.name.clone())]);
    if let Some(owners) = object["metadata"]["ownerReferences"].as_array() {
        for owner in owners {
            if let Some(uid) = owner["uid"].as_str() {
                result.insert(Index::Owner(uid.into()));
            }
        }
    }
    if let Some(node) = object["spec"]["nodeName"]
        .as_str()
        .or_else(|| object["status"]["nodeName"].as_str())
    {
        result.insert(Index::Node(node.into()));
    }
    if let Some(volumes) = object["spec"]["volumes"].as_array() {
        for volume in volumes {
            if let Some(claim) = volume["persistentVolumeClaim"]["claimName"].as_str() {
                result.insert(Index::Claim(key.namespace.clone(), claim.into()));
            }
            if !volume["ephemeral"].is_null() {
                if let Some(name) = volume["name"].as_str() {
                    result.insert(Index::Claim(key.namespace.clone(), format!("{}-{name}", key.name)));
                }
            }
        }
    }
    if matches!(object["kind"].as_str(), Some("PersistentVolume" | "PersistentVolumeClaim")) {
        result.insert(Index::StorageClass(object["spec"]["storageClassName"].as_str().unwrap_or("").into()));
    }
    if let (Some(ns), Some(name)) = (object["spec"]["claimRef"]["namespace"].as_str(), object["spec"]["claimRef"]["name"].as_str()) {
        result.insert(Index::Claim(ns.into(), name.into()));
    }
    for volume in [object["spec"]["volumeName"].as_str(), object["spec"]["source"]["persistentVolumeName"].as_str()].into_iter().flatten() {
        result.insert(Index::Volume(volume.into()));
    }
    if let Some(driver) = object["spec"]["csi"]["driver"].as_str() { result.insert(Index::Driver(driver.into())); }
    if let Some(pod) = object["spec"]["podName"].as_str() { result.insert(Index::Pod(key.namespace.clone(), pod.into())); }
    if let Some(class) = object["storageClassName"].as_str() { result.insert(Index::StorageClass(class.into())); }
    if let Some(name) = object["spec"]["scaleTargetRef"]["name"].as_str() {
        result.insert(Index::Target(key.namespace.clone(),object["spec"]["scaleTargetRef"]["kind"].as_str().unwrap_or("Deployment").into(),name.into()));
    }
    for field in ["sourceNode", "targetNode"] {
        if let Some(node) = object["spec"][field].as_str() { result.insert(Index::Node(node.into())); }
    }
    if let Some(class) = object["spec"]["gatewayClassName"].as_str() {
        result.insert(Index::Reference("".into(),"GatewayClass".into(),class.into()));
    }
    for parent in object["spec"]["parentRefs"].as_array().into_iter().flatten() {
        if let Some(name) = parent["name"].as_str() {
            result.insert(Index::Reference(parent["namespace"].as_str().unwrap_or(&key.namespace).into(),
                parent["kind"].as_str().unwrap_or("Gateway").into(),name.into()));
        }
    }
    for rule in object["spec"]["rules"].as_array().into_iter().flatten() {
        for backend in rule["backendRefs"].as_array().into_iter().flatten() {
            if let Some(name) = backend["name"].as_str() {
                result.insert(Index::Reference(backend["namespace"].as_str().unwrap_or(&key.namespace).into(),
                    backend["kind"].as_str().unwrap_or("Service").into(),name.into()));
            }
        }
    }
    if let Some(labels) = object["metadata"]["labels"].as_object() {
        for (label, value) in labels {
            if let Some(value) = value.as_str() {
                result.insert(Index::Label(label.clone(), value.into()));
            }
        }
    }
    if let Some(selector) = pod_selector(object) {
        let anchors = selector_anchors(&selector);
        if anchors.is_empty() {
            result.insert(Index::SelectorFallback(key.namespace.clone()));
        } else {
            for (label, value) in anchors {
                result.insert(Index::Selector(key.namespace.clone(), label, value));
            }
        }
    }
    result
}

/// Normalize the two Pod selector schemas without conflating absent and empty.
pub fn pod_selector(object: &Value) -> Option<Value> {
    let selector = &object["spec"]["selector"];
    if !selector.is_object() { return None; }
    match object["kind"].as_str() {
        Some("Service") => Some(serde_json::json!({"matchLabels": selector})),
        Some("PodDisruptionBudget") => Some(selector.clone()),
        _ => None,
    }
}

/// One required positive clause suffices as an inverse-index anchor. Negative
/// and empty selectors use a namespace-local fallback, then exact matching.
pub fn selector_anchors(selector: &Value) -> Vec<(String, String)> {
    if let Some(labels) = selector["matchLabels"].as_object() {
        if let Some((key, value)) = labels.iter().find(|(_, value)| value.is_string()) {
            return vec![(key.clone(), value.as_str().unwrap().into())];
        }
    }
    for expr in selector["matchExpressions"].as_array().into_iter().flatten() {
        if expr["operator"] == "In" {
            if let (Some(key), Some(values)) = (expr["key"].as_str(), expr["values"].as_array()) {
                return values.iter().filter_map(|v| v.as_str().map(|v| (key.into(), v.into()))).collect();
            }
        }
    }
    Vec::new()
}

#[derive(Clone, Debug)]
pub struct Delta {
    pub old: Option<Value>,
    pub new: Option<Value>,
    /// Union of both sides: moving an object must wake its previous dependents.
    pub affected: HashSet<Index>,
}

#[derive(Default)]
pub struct Store {
    synced: bool,
    initialized: bool,
    objects: HashMap<Key, Value>,
    names: HashMap<(String, String), Key>,
    uids: HashMap<String, Key>,
    indexes: HashMap<Index, HashSet<Key>>,
    /// Successful writes are visible locally until their exact watch event.
    overlays: HashMap<Key, Value>,
    seen: VecDeque<(String, String)>,
    acknowledged_at: HashMap<Key, std::time::Instant>,
}
impl Store {
    pub fn unavailable(&mut self) {
        self.synced = false;
    }
    pub fn resumed(&mut self) {
        self.synced = self.initialized;
    }
    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// Atomically replace a complete snapshot. The caller must serialize this
    /// with writes; a LIST begun before an acknowledgement cannot clear it.
    pub fn reset(&mut self, list: &Value) -> anyhow::Result<Vec<Delta>> {
        let items = list["items"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("not a LIST"))?;
        let mut replacement = Store::default();
        replacement.seen = self.seen.clone();
        for object in items {
            replacement.put(object.clone())?;
            if let Some(rv) = revision(object) {
                replacement
                    .seen
                    .push_back((Key::of(object)?.uid, rv.into()));
            }
        }
        while replacement.seen.len() > 4096 {
            replacement.seen.pop_front();
        }
        // Unacknowledged writes survive relists; clearing requires the exact
        // revision or a separately established read-after-write barrier.
        for (key, object) in &self.overlays {
            if replacement
                .objects
                .get(key)
                .is_some_and(|v| revision(v) == revision(object))
            {
                continue;
            }
            replacement.put(object.clone())?;
            replacement.overlays.insert(key.clone(), object.clone());
            if let Some(at) = self.acknowledged_at.get(key) {
                replacement.acknowledged_at.insert(key.clone(), *at);
            }
        }
        let mut changes = Vec::new();
        for (key, old) in &self.objects {
            let new = replacement.objects.get(key);
            if new != Some(old) {
                changes.push(delta(Some(old.clone()), new.cloned())?);
            }
        }
        for (key, new) in &replacement.objects {
            if !self.objects.contains_key(key) {
                changes.push(delta(None, Some(new.clone()))?);
            }
        }
        replacement.synced = true;
        replacement.initialized = true;
        *self = replacement;
        Ok(changes)
    }

    /// Use only for a LIST started after all outstanding writes completed.
    /// The caller must hold its write-generation barrier through this commit.
    pub fn reset_after_writes(&mut self, list: &Value) -> anyhow::Result<Vec<Delta>> {
        let items = list["items"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("not a LIST"))?;
        for item in items {
            Key::of(item)?;
        }
        self.overlays.clear();
        self.acknowledged_at.clear();
        self.reset(list)
    }

    /// A snapshot begun after a successful write contains that write or a
    /// later durable state. Preserve only writes acknowledged during the LIST.
    pub fn reset_started_at(
        &mut self,
        list: &Value,
        started: std::time::Instant,
    ) -> anyhow::Result<Vec<Delta>> {
        let items = list["items"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("not a LIST"))?;
        for item in items {
            Key::of(item)?;
        }
        self.overlays.retain(|key, _| {
            self.acknowledged_at
                .get(key)
                .is_some_and(|at| *at >= started)
        });
        self.acknowledged_at
            .retain(|key, _| self.overlays.contains_key(key));
        self.reset(list)
    }

    pub fn acknowledge(&mut self, object: Value) -> anyhow::Result<Option<Delta>> {
        let key = Key::of(&object)?;
        let rv =
            revision(&object).ok_or_else(|| anyhow::anyhow!("write response has no revision"))?;
        if self
            .seen
            .iter()
            .any(|(uid, seen)| uid == &key.uid && seen == rv)
        {
            return Ok(None);
        }
        let change = self.put(object.clone())?;
        self.acknowledged_at
            .insert(key.clone(), std::time::Instant::now());
        self.overlays.insert(key, object);
        Ok(Some(change))
    }

    pub fn apply(&mut self, event: &Value) -> anyhow::Result<Option<Delta>> {
        let object = &event["object"];
        let kind = event["type"].as_str().unwrap_or("");
        if kind == "BOOKMARK" {
            return Ok(None);
        }
        let key = Key::of(object)?;
        let rv = revision(object).ok_or_else(|| anyhow::anyhow!("watch has no revision"))?;
        self.seen.push_back((key.uid.clone(), rv.into()));
        while self.seen.len() > 4096 {
            self.seen.pop_front();
        }
        if let Some(overlay) = self.overlays.get(&key) {
            if revision(overlay) != Some(rv) {
                return Ok(None);
            }
            self.overlays.remove(&key);
            self.acknowledged_at.remove(&key);
        }
        match kind {
            "ADDED" | "MODIFIED" => self.put(object.clone()).map(Some),
            "DELETED" => {
                let old = self.remove(&key);
                old.map(|old| delta(Some(old), None)).transpose()
            }
            _ => anyhow::bail!("invalid watch event {kind}"),
        }
    }

    pub fn get(&self, key: &Key) -> anyhow::Result<Option<&Value>> {
        anyhow::ensure!(self.synced, "informer is not synchronized");
        Ok(self.objects.get(key))
    }
    pub fn key_for_uid(&self, uid: &str) -> Option<Key> {
        if self.synced {
            self.uids.get(uid).cloned()
        } else {
            None
        }
    }
    pub fn select(&self, index: &Index) -> anyhow::Result<Vec<Value>> {
        anyhow::ensure!(self.synced, "informer is not synchronized");
        Ok(self
            .indexes
            .get(index)
            .into_iter()
            .flatten()
            .filter_map(|key| self.objects.get(key).cloned())
            .collect())
    }
    pub fn values(&self) -> anyhow::Result<Vec<Value>> {
        anyhow::ensure!(self.synced, "informer is not synchronized");
        Ok(self.objects.values().cloned().collect())
    }
    pub fn keys(&self) -> anyhow::Result<Vec<Key>> {
        anyhow::ensure!(self.synced, "informer is not synchronized");
        Ok(self.objects.keys().cloned().collect())
    }
    fn remove(&mut self, key: &Key) -> Option<Value> {
        let old = self.objects.remove(key)?;
        self.uids.remove(&key.uid);
        if self.names.get(&key.name_key()) == Some(key) {
            self.names.remove(&key.name_key());
        }
        for index in indexes(&old, key) {
            if let Some(keys) = self.indexes.get_mut(&index) {
                keys.remove(key);
                if keys.is_empty() {
                    self.indexes.remove(&index);
                }
            }
        }
        Some(old)
    }
    fn put(&mut self, object: Value) -> anyhow::Result<Delta> {
        let key = Key::of(&object)?;
        let previous = self.names.get(&key.name_key()).cloned();
        let old = previous.and_then(|old| {
            if old != key {
                self.overlays.remove(&old);
                self.acknowledged_at.remove(&old);
            }
            self.remove(&old)
        });
        for index in indexes(&object, &key) {
            self.indexes.entry(index).or_default().insert(key.clone());
        }
        self.names.insert(key.name_key(), key.clone());
        self.uids.insert(key.uid.clone(), key.clone());
        self.objects.insert(key, object.clone());
        delta(old, Some(object))
    }
}
fn revision(object: &Value) -> Option<&str> {
    object["metadata"]["resourceVersion"].as_str()
}
fn delta(old: Option<Value>, new: Option<Value>) -> anyhow::Result<Delta> {
    let mut affected = HashSet::new();
    for object in old.iter().chain(new.iter()) {
        affected.extend(indexes(object, &Key::of(object)?));
    }
    Ok(Delta { old, new, affected })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn object(uid: &str, rv: &str, owner: &str) -> Value {
        json!({"metadata":{"namespace":"ns","name":"pod","uid":uid,
            "resourceVersion":rv,"ownerReferences":[{"uid":owner}]},
            "spec":{"nodeName":"node-a"}})
    }
    fn event(kind: &str, object: Value) -> Value {
        json!({"type":kind,"object":object})
    }

    #[test]
    fn selector_indexes_preserve_negative_empty_and_old_membership() {
        let service = serde_json::json!({"kind":"Service", "metadata":{"name":"web","namespace":"ns","uid":"svc"},
            "spec":{"selector":{"app":"web"}}});
        let pdb = serde_json::json!({"kind":"PodDisruptionBudget", "metadata":{"name":"budget","namespace":"ns","uid":"pdb"},
            "spec":{"selector":{"matchExpressions":[{"key":"env","operator":"NotIn","values":["dev"]}]}}});
        let mut store = Store::default();
        store.reset(&json!({"items":[service.clone(),pdb]})).unwrap();
        assert_eq!(store.select(&Index::Selector("ns".into(),"app".into(),"web".into())).unwrap().len(), 1);
        assert_eq!(store.select(&Index::SelectorFallback("ns".into())).unwrap().len(), 1);
        assert!(store.select(&Index::Selector("other".into(),"app".into(),"web".into())).unwrap().is_empty());
        let mut changed = service;
        changed["metadata"]["resourceVersion"] = json!("opaque");
        changed["spec"]["selector"]["app"] = json!("api");
        let change = store.apply(&event("MODIFIED", changed)).unwrap().unwrap();
        assert!(change.affected.contains(&Index::Selector("ns".into(),"app".into(),"web".into())));
        assert!(store.select(&Index::Selector("ns".into(),"app".into(),"web".into())).unwrap().is_empty());
        assert_eq!(store.select(&Index::Selector("ns".into(),"app".into(),"api".into())).unwrap().len(), 1);
        assert_eq!(selector_anchors(&json!({"matchExpressions":[{"key":"env","operator":"In","values":["prod","stage"]}]})).len(), 2);
        assert!(selector_anchors(&json!({})).is_empty());
    }

    #[test]
    fn moving_dependencies_routes_old_and_new_owners() {
        let mut store = Store::default();
        store
            .reset(&json!({"items":[object("uid","opaque-a","a")]}))
            .unwrap();
        let change = store
            .apply(&event("MODIFIED", object("uid", "opaque-b", "b")))
            .unwrap()
            .unwrap();
        assert!(change.affected.contains(&Index::Owner("a".into())));
        assert!(change.affected.contains(&Index::Owner("b".into())));
        assert!(store.select(&Index::Owner("a".into())).unwrap().is_empty());
        assert_eq!(store.select(&Index::Owner("b".into())).unwrap().len(), 1);
    }

    #[test]
    fn old_uid_deletion_cannot_remove_recreated_name() {
        let mut store = Store::default();
        store
            .reset(&json!({"items":[object("old","a","owner")]}))
            .unwrap();
        store
            .apply(&event("ADDED", object("new", "b", "owner")))
            .unwrap();
        store
            .apply(&event("DELETED", object("old", "c", "owner")))
            .unwrap();
        assert_eq!(store.keys().unwrap()[0].uid, "new");
    }

    #[test]
    fn write_acknowledgement_survives_lag_but_not_a_later_watch_update() {
        let mut store = Store::default();
        store.reset(&json!({"items":[]})).unwrap();
        let created = object("uid", "write", "owner");
        store.acknowledge(created.clone()).unwrap();
        store.reset(&json!({"items":[]})).unwrap();
        assert_eq!(store.keys().unwrap().len(), 1);
        store.apply(&event("ADDED", created)).unwrap();
        store
            .apply(&event("MODIFIED", object("uid", "later", "changed")))
            .unwrap();
        assert_eq!(
            store.select(&Index::Owner("changed".into())).unwrap().len(),
            1
        );
        // The HTTP response can arrive after the watch already applied it.
        assert!(store
            .acknowledge(object("uid", "write", "owner"))
            .unwrap()
            .is_none());
        assert_eq!(
            store.select(&Index::Owner("changed".into())).unwrap().len(),
            1
        );
    }

    #[test]
    fn a_relist_clears_only_writes_known_to_precede_its_read_barrier() {
        let mut store = Store::default();
        store.reset(&json!({"items":[]})).unwrap();
        let before_write = std::time::Instant::now();
        store.acknowledge(object("uid", "write", "owner")).unwrap();
        store
            .reset_started_at(&json!({"items":[]}), before_write)
            .unwrap();
        assert_eq!(store.keys().unwrap().len(), 1);
        let after_write = std::time::Instant::now() + std::time::Duration::from_secs(1);
        store
            .reset_started_at(&json!({"items":[]}), after_write)
            .unwrap();
        assert!(store.keys().unwrap().is_empty());
    }

    #[test]
    fn failed_observation_never_becomes_an_empty_authoritative_view() {
        let mut store = Store::default();
        assert!(store.keys().is_err());
        store
            .reset(&json!({"items":[object("uid","a","owner")]}))
            .unwrap();
        store.unavailable();
        assert!(store.keys().is_err());
        assert!(store.reset(&json!({"kind":"Status","code":403})).is_err());
        assert!(store.keys().is_err());
        store.reset_after_writes(&json!({"items":[]})).unwrap();
        assert!(store.keys().unwrap().is_empty());
    }
}
