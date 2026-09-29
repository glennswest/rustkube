//! Bounded object workers for controllers whose dependencies are owned objects.
use crate::runner::ApiClient;
use apimachinery::informer::{Index, Key};
use apimachinery::workqueue::WorkQueue;
use futures::{stream::FuturesUnordered, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

#[async_trait::async_trait]
pub trait Controller: Send + Sync {
    fn name(&self) -> &'static str;
    fn primary(&self) -> &'static str;
    fn children(&self) -> &'static str;
    async fn reconcile(&self, object: &Value, children: &[Value]) -> anyhow::Result<()>;
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
    let changed = ready.clone();
    let owners = primary.feed.clone();
    let children = api.informers.subscribe(
        &api.client,
        format!("{}{}", api.base_url, controller.children()),
        move |changes, reset| {
            if reset {
                if let Ok(keys) = owners.keys() {
                    for key in keys {
                        changed.add(key);
                    }
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
    );
    if let Ok(keys) = primary.feed.keys() {
        for key in keys {
            ready.add(key);
        }
    }
    let mut active = FuturesUnordered::new();
    let mut failures = HashMap::<Key, u32>::new();
    loop {
        tokio::select! {
            work = ready.next(), if active.len() < 8 => {
                ready.cancel_deadline(work.key());
                let primary = primary.feed.clone();
                let children = children.feed.clone();
                let queue = ready.clone();
                let key = work.key().clone();
                active.push(async move {
                    let (result,failed) = apimachinery::reactor::scope_object(move |delay| {
                        queue.add_at(key.clone(),tokio::time::Instant::now()+delay);
                    },async {
                        let Some(object) = primary.get(work.key())? else { return Ok(()) };
                        let children = children.select(&Index::Owner(work.key().uid.clone()))?;
                        controller.reconcile(&object,&children).await
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
