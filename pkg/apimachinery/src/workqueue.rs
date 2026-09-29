//! Deduplicated, cancellation-safe async work queue. No periodic wakeups.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::time::Instant;

#[derive(Default)]
struct Entry {
    queued: bool,
    processing: bool,
    dirty: bool,
    deadline: Option<Instant>,
}

struct State<K> {
    ready: VecDeque<K>,
    entries: HashMap<K, Entry>,
    deadlines: BTreeMap<Instant, HashSet<K>>,
}

/// A key occupies at most one ready slot and one worker. Events during work
/// request exactly one subsequent pass. A deadline is independent of dirty
/// state, so an early event cannot erase a semantic timeout.
pub struct WorkQueue<K> {
    state: Mutex<State<K>>,
    wake: Notify,
}

impl<K: Clone + Eq + Hash> WorkQueue<K> {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                ready: VecDeque::new(),
                entries: HashMap::new(),
                deadlines: BTreeMap::new(),
            }),
            wake: Notify::new(),
        })
    }

    pub fn add(&self, key: K) {
        let mut state = self.state.lock().unwrap();
        let entry = state.entries.entry(key.clone()).or_default();
        if entry.processing {
            entry.dirty = true;
        } else if !entry.queued {
            entry.queued = true;
            state.ready.push_back(key);
        }
        drop(state);
        self.wake.notify_one();
    }

    /// Keep the earliest requested deadline. No task is spawned per timer.
    pub fn add_at(&self, key: K, deadline: Instant) {
        let mut state = self.state.lock().unwrap();
        let entry = state.entries.entry(key.clone()).or_default();
        let old = entry.deadline;
        if old.is_some_and(|old| old <= deadline) {
            return;
        }
        entry.deadline = Some(deadline);
        if let Some(old) = old {
            Self::remove_deadline(&mut state, &key, old);
        }
        state.deadlines.entry(deadline).or_default().insert(key);
        drop(state);
        self.wake.notify_one();
    }

    /// Remove a semantic deadline after recomputing it from current state.
    pub fn cancel_deadline(&self, key: &K) {
        let mut state = self.state.lock().unwrap();
        let old = state
            .entries
            .get_mut(key)
            .and_then(|entry| entry.deadline.take());
        if let Some(old) = old {
            Self::remove_deadline(&mut state, key, old);
        }
        if let Some(entry) = state.entries.get_mut(key) {
            if !entry.queued && !entry.processing {
                state.entries.remove(key);
            }
        }
        drop(state);
        self.wake.notify_one();
    }

    pub async fn next(self: &Arc<Self>) -> Work<K> {
        self.next_by(|_,_| std::cmp::Ordering::Equal).await
    }

    /// Select a ready key by priority without changing deduplication or the
    /// processing/dirty protocol. Equal priorities retain FIFO order.
    pub async fn next_by(self: &Arc<Self>, compare: impl Fn(&K,&K) -> std::cmp::Ordering) -> Work<K> {
        loop {
            // enable before examining state: supports multiple waiting workers
            // without losing notify_one between the check and the await.
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let deadline = {
                let mut state = self.state.lock().unwrap();
                let now = Instant::now();
                while state
                    .deadlines
                    .first_key_value()
                    .is_some_and(|(when, _)| *when <= now)
                {
                    let (_, due) = state.deadlines.pop_first().unwrap();
                    for key in due {
                        let entry = state.entries.get_mut(&key).unwrap();
                        entry.deadline = None;
                        if entry.processing {
                            entry.dirty = true;
                        } else if !entry.queued {
                            entry.queued = true;
                            state.ready.push_back(key);
                        }
                    }
                }
                let next = state.ready.iter().enumerate().min_by(|(_,a),(_,b)| compare(a,b)).map(|(i,_)| i);
                if let Some(key) = next.and_then(|i| state.ready.remove(i)) {
                    let entry = state.entries.get_mut(&key).unwrap();
                    entry.queued = false;
                    entry.processing = true;
                    return Work {
                        queue: self.clone(),
                        key,
                    };
                }
                state.deadlines.first_key_value().map(|(when, _)| *when)
            };
            match deadline {
                Some(when) => tokio::select! {
                    _ = notified => {},
                    _ = tokio::time::sleep_until(when) => {},
                },
                None => notified.await,
            }
        }
    }

    fn remove_deadline(state: &mut State<K>, key: &K, when: Instant) {
        if let Some(keys) = state.deadlines.get_mut(&when) {
            keys.remove(key);
            if keys.is_empty() {
                state.deadlines.remove(&when);
            }
        }
    }

    fn done(&self, key: &K) {
        let mut state = self.state.lock().unwrap();
        let entry = state.entries.get_mut(key).expect("processing key missing");
        entry.processing = false;
        if entry.dirty {
            entry.dirty = false;
            entry.queued = true;
            state.ready.push_back(key.clone());
        } else if entry.deadline.is_none() {
            state.entries.remove(key);
        }
        drop(state);
        self.wake.notify_one();
    }
}

/// Dropping the work guard releases ownership, including on task cancellation.
pub struct Work<K: Clone + Eq + Hash> {
    queue: Arc<WorkQueue<K>>,
    key: K,
}
impl<K: Clone + Eq + Hash> Work<K> {
    pub fn key(&self) -> &K {
        &self.key
    }
}
impl<K: Clone + Eq + Hash> Drop for Work<K> {
    fn drop(&mut self) {
        self.queue.done(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn duplicate_and_processing_events_coalesce_without_losing_work() {
        let q = WorkQueue::new();
        q.add("a");
        q.add("a");
        q.add("b");
        let a = q.next().await;
        assert_eq!(a.key(), &"a");
        q.add("a");
        q.add("a");
        let b = q.next().await;
        assert_eq!(b.key(), &"b");
        drop(a);
        drop(b);
        assert_eq!(q.next().await.key(), &"a");
        assert!(tokio::time::timeout(Duration::from_millis(20), q.next())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn rescheduling_and_cancellation_remove_old_deadline_entries() {
        let q = WorkQueue::new();
        let now = Instant::now();
        q.add_at("a", now + Duration::from_secs(300));
        q.add_at("a", now + Duration::from_secs(200));
        q.add_at("a", now + Duration::from_secs(400));
        q.add_at("b", now + Duration::from_secs(200));
        {
            let state = q.state.lock().unwrap();
            assert_eq!(state.deadlines.len(), 1);
            assert_eq!(state.deadlines.first_key_value().unwrap().1.len(), 2);
        }
        q.cancel_deadline(&"a");
        q.cancel_deadline(&"b");
        {
            let state = q.state.lock().unwrap();
            assert!(state.deadlines.is_empty());
            assert!(state.entries.is_empty());
        }
        q.add("ready");
        assert_eq!(q.next().await.key(), &"ready");
    }

    #[tokio::test]
    async fn a_new_event_interrupts_a_distant_deadline() {
        let q = WorkQueue::new();
        q.add_at("later", Instant::now() + Duration::from_secs(300));
        let reader = q.clone();
        let task = tokio::spawn(async move { reader.next().await });
        tokio::task::yield_now().await;
        q.add("now");
        let work = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.key(), &"now");
    }

    #[tokio::test]
    async fn deadlines_are_not_erased_by_an_early_event() {
        let q = WorkQueue::new();
        q.add_at("a", Instant::now() + Duration::from_millis(10));
        q.add("a");
        drop(q.next().await);
        let due = tokio::time::timeout(Duration::from_secs(1), q.next())
            .await
            .unwrap();
        assert_eq!(due.key(), &"a");
    }

    #[tokio::test]
    async fn cancelled_worker_releases_ownership_and_keeps_dirty_work() {
        let q = WorkQueue::new();
        q.add(1);
        let reader = q.clone();
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _work = reader.next().await;
            let _ = ready.send(());
            std::future::pending::<()>().await;
        });
        started.await.unwrap();
        q.add(1);
        task.abort();
        let _ = task.await;
        assert_eq!(
            *tokio::time::timeout(Duration::from_millis(100), q.next())
                .await
                .unwrap()
                .key(),
            1
        );
    }
}
