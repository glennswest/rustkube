//! Bounded object workers for controllers whose dependencies are owned objects
//! or other watched collections routed through inverse indexes.
use crate::runner::ApiClient;
use apimachinery::informer::{Delta, Index, Key};
use apimachinery::informers::Feed;
use apimachinery::workqueue::WorkQueue;
use futures::{stream::FuturesUnordered, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Maps one change of a dependency collection to the primary objects it
/// affects. `primary` is the controller's own (synchronized) feed.
pub type Route = Arc<dyn Fn(&Delta, &Feed) -> Vec<Key> + Send + Sync>;

/// A watched collection besides the primary and its owned children.
pub struct Dependency {
    pub path: &'static str,
    pub route: Route,
}

/// Read access to the dependency feeds, in `Controller::dependencies` order.
/// Every read fails while its feed is unsynchronized — never an empty view.
pub struct Deps {
    feeds: Vec<Arc<Feed>>,
}
impl Deps {
    pub fn feed(&self, i: usize) -> &Feed {
        &self.feeds[i]
    }
}

#[async_trait::async_trait]
pub trait Controller: Send + Sync {
    fn name(&self) -> &'static str;
    fn primary(&self) -> &'static str;
    /// Collection whose members name the primary as an owner, if any.
    fn children(&self) -> Option<&'static str> {
        None
    }
    fn dependencies(&self) -> Vec<Dependency> {
        Vec::new()
    }
    /// Distinct primary keys reconciled at once.
    fn workers(&self) -> usize {
        8
    }
    async fn reconcile(&self, object: &Value, children: &[Value], deps: &Deps)
        -> anyhow::Result<()>;
}

/// Every primary key: for a dependency relist, whose lost events are unknown.
pub fn all_keys(primary: &Feed) -> Vec<Key> {
    primary.keys().unwrap_or_default()
}

/// Primary keys in the namespaces a change touched (old and new side).
pub fn same_namespace(delta: &Delta, primary: &Feed) -> Vec<Key> {
    let mut keys = Vec::new();
    for index in &delta.affected {
        if let Index::Namespace(_) = index {
            for object in primary.select(index).unwrap_or_default() {
                if let Ok(key) = Key::of(&object) {
                    keys.push(key);
                }
            }
        }
    }
    keys
}

pub async fn run(api: &ApiClient, controller: &dyn Controller) {
    let ready = WorkQueue::<Key>::new();
    let changed = ready.clone();
    let primary = api.informers.subscribe(
        &api.client,
        format!("{}{}", api.base_url, controller.primary()),
        move |changes, _| {
            for change in changes {
                for object in change.old.iter().chain(change.new.iter()) {
                    if let Ok(key) = Key::of(object) {
                        changed.add(key);
                    }
                }
            }
        },
    );
    let children = controller.children().map(|path| {
        let changed = ready.clone();
        let owners = primary.feed.clone();
        api.informers.subscribe(
            &api.client,
            format!("{}{}", api.base_url, path),
            move |changes, reset| {
                if reset {
                    for key in all_keys(&owners) {
                        changed.add(key);
                    }
                }
                for change in changes {
                    for index in &change.affected {
                        if let Index::Owner(uid) = index {
                            if let Some(key) = owners.key_for_uid(uid) {
                                changed.add(key);
                            }
                        }
                    }
                }
            },
        )
    });
    // Subscriptions live as long as this worker term; dropping them on
    // cancellation stops the watches with their last subscriber.
    let dependencies: Vec<_> = controller
        .dependencies()
        .into_iter()
        .map(|dependency| {
            let changed = ready.clone();
            let owners = primary.feed.clone();
            let route = dependency.route;
            api.informers.subscribe(
                &api.client,
                format!("{}{}", api.base_url, dependency.path),
                move |changes, reset| {
                    if reset {
                        for key in all_keys(&owners) {
                            changed.add(key);
                        }
                    }
                    for change in changes {
                        for key in route(change, &owners) {
                            changed.add(key);
                        }
                    }
                },
            )
        })
        .collect();
    let deps = Arc::new(Deps {
        feeds: dependencies.iter().map(|s| s.feed.clone()).collect(),
    });
    for key in all_keys(&primary.feed) {
        ready.add(key);
    }
    let limit = controller.workers().max(1);
    let mut active = FuturesUnordered::new();
    let mut failures = HashMap::<Key, u32>::new();
    loop {
        tokio::select! {
            work = ready.next(), if active.len() < limit => {
                ready.cancel_deadline(work.key());
                let primary = primary.feed.clone();
                let children = children.as_ref().map(|c| c.feed.clone());
                let deps = deps.clone();
                let queue = ready.clone();
                let key = work.key().clone();
                active.push(async move {
                    let (result,failed) = apimachinery::reactor::scope_object(move |delay| {
                        queue.add_at(key.clone(),tokio::time::Instant::now()+delay);
                    },async {
                        let Some(object) = primary.get(work.key())? else { return Ok(()) };
                        let children = match &children {
                            Some(feed) => feed.select(&Index::Owner(work.key().uid.clone()))?,
                            None => Vec::new(),
                        };
                        controller.reconcile(&object,&children,&deps).await
                    }).await;
                    (work,result,failed)
                });
            }
            Some((work,result,failed)) = active.next(), if !active.is_empty() => {
                if failed || result.is_err() {
                    let attempts = failures.entry(work.key().clone()).or_default();
                    *attempts = attempts.saturating_add(1);
                    tracing::warn!(controller=controller.name(),key=?work.key(),error=?result.err(),"object reconciliation failed");
                    ready.add_at(work.key().clone(),tokio::time::Instant::now()+Duration::from_millis((100_u64 << (*attempts).min(8)).min(30_000)));
                } else { failures.remove(work.key()); }
                drop(work);
            }
        }
    }
}
