//! Datastore compaction, as upstream kube-apiserver does it (#139).
//!
//! Nothing compacted fastetcd: its own auto-compaction is off unless a flag
//! turns it on, and its space reclaim cuts history only past 80% full. So
//! revision history grew without bound, and a LIST continue token never
//! expired — upstream's paging contract (a 410 with an "inconsistent" token
//! once the snapshot is gone) could not happen.
//!
//! Every `--etcd-compaction-interval` each apiserver runs one round of
//! upstream's `compact.go`: a transaction on `compact_rev_key` that writes
//! only if the key's version is still the one this apiserver last saw. One
//! apiserver wins per interval; it compacts to the revision it recorded on
//! its previous win, so every snapshot younger than one interval stays
//! readable and every one older than two is gone. The others learn the new
//! version and try again next interval. The winner's own write is a new
//! revision, so a token taken before it is always below the next compaction.

use apimachinery::store::KvStore;
use std::sync::Arc;
use std::time::Duration;

/// The coordination key — upstream's, so a mixed fleet would agree.
pub const COMPACT_REV_KEY: &str = "compact_rev_key";

/// What an apiserver remembers between rounds: the key's version it last
/// saw, and the store revision of that round (the next win's target).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Round {
    pub seen: i64,
    pub revision: u64,
}

/// One round. Returns the state for the next, and the revision compacted
/// to, if this round compacted. On an error after the claim, the state still
/// advances (as upstream's does): the claim was made.
pub async fn round(store: &dyn KvStore, last: Round) -> (Round, apimachinery::Result<Option<u64>>) {
    let (won, seen, revision) = match store.compact_claim(COMPACT_REV_KEY, last.seen, &last.revision.to_string()).await {
        Ok(claim) => claim,
        Err(e) => return (last, Err(e)),
    };
    let next = Round { seen, revision };
    // Lost: another apiserver compacted this interval. Bootstrap (no
    // revision recorded yet): only claim.
    if !won || last.revision == 0 {
        return (next, Ok(None));
    }
    match store.compact(last.revision).await {
        Ok(()) => (next, Ok(Some(last.revision))),
        Err(e) => (next, Err(e)),
    }
}

/// A Go duration as upstream's flag takes it: `5m`, `5m0s`, `1h30m`,
/// `300s`, `0`. Units h, m, s, ms; whole numbers.
pub fn parse_interval(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    if text == "0" {
        return Ok(Duration::ZERO);
    }
    let bad = || format!("invalid duration {text:?}: use e.g. 5m, 5m0s, 300s, 0");
    let (mut total, mut rest) = (Duration::ZERO, text);
    if rest.is_empty() {
        return Err(bad());
    }
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let n: u64 = rest[..digits].parse().map_err(|_| bad())?;
        rest = &rest[digits..];
        let unit = rest.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
        let ms = match &rest[..unit] {
            "h" => 3_600_000,
            "m" => 60_000,
            "s" => 1_000,
            "ms" => 1,
            _ => return Err(bad()),
        };
        rest = &rest[unit..];
        total += Duration::from_millis(n.checked_mul(ms).ok_or_else(bad)?);
    }
    Ok(total)
}

/// Compact every `interval`; zero turns it off.
pub fn spawn(store: Arc<dyn KvStore>, interval: Duration) {
    if interval.is_zero() {
        tracing::info!("datastore compaction off (--etcd-compaction-interval=0)");
        return;
    }
    tokio::spawn(async move {
        let mut state = Round::default();
        loop {
            tokio::time::sleep(interval).await;
            let (next, outcome) = round(store.as_ref(), state).await;
            state = next;
            match outcome {
                Ok(Some(rev)) => {
                    metrics::gauge!("apiserver_storage_compacted_revision").set(rev as f64);
                    tracing::info!(revision = rev, "datastore compacted");
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "datastore compaction failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use apimachinery::store::{LeaseId, ListResult, WatchStream};
    use apimachinery::{Error, Result};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// The compact key and the compaction floor, with a revision counter.
    #[derive(Default)]
    struct Fake {
        inner: Mutex<(u64, i64, u64)>, // (revision, key version, compacted to)
    }

    #[async_trait]
    impl KvStore for Fake {
        async fn get(&self, _: &str) -> Result<Option<(Vec<u8>, u64)>> {
            Ok(None)
        }
        async fn put(&self, _: &str, _: &[u8], _: Option<u64>) -> Result<u64> {
            let mut g = self.inner.lock().unwrap();
            g.0 += 1;
            Ok(g.0)
        }
        async fn delete(&self, _: &str, _: Option<u64>) -> Result<u64> {
            unimplemented!()
        }
        async fn list(&self, _: &str, _: usize, _: Option<&str>) -> Result<ListResult> {
            unimplemented!()
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
        async fn compact(&self, revision: u64) -> Result<()> {
            let mut g = self.inner.lock().unwrap();
            if revision > g.0 {
                return Err(Error::Store("future revision".into()));
            }
            g.2 = g.2.max(revision);
            Ok(())
        }
        async fn compact_claim(&self, _: &str, seen: i64, _: &str) -> Result<(bool, i64, u64)> {
            let mut g = self.inner.lock().unwrap();
            if g.1 != seen {
                return Ok((false, g.1, g.0));
            }
            g.0 += 1;
            g.1 += 1;
            Ok((true, g.1, g.0))
        }
    }

    #[tokio::test]
    async fn one_apiserver_compacts_per_interval_to_the_revision_of_its_last_win() {
        let store = Fake::default();
        let (mut a, mut b) = (Round::default(), Round::default());

        // Interval 1: a claims at bootstrap and compacts nothing; b, having
        // seen an older version, loses and learns the current one.
        let (next, out) = round(&store, a).await;
        assert_eq!(out.unwrap(), None);
        a = next;
        let (next, out) = round(&store, b).await;
        assert_eq!(out.unwrap(), None);
        b = next;
        assert_eq!(store.inner.lock().unwrap().2, 0, "nothing compacted at bootstrap");

        // Writes happen; a token is taken at revision 4.
        for _ in 0..3 {
            store.put("/registry/x", b"", None).await.unwrap();
        }
        let token_rev = store.inner.lock().unwrap().0;

        // Interval 2: b now holds the current version, wins, and compacts to
        // the revision it recorded when it lost — the bootstrap claim's.
        let (next, out) = round(&store, b).await;
        assert_eq!(out.unwrap(), Some(1));
        b = next;
        let (next, out) = round(&store, a).await;
        assert_eq!(out.unwrap(), None, "a lost interval 2");
        a = next;
        assert!(store.inner.lock().unwrap().2 < token_rev, "a one-interval-old token still reads");

        // Interval 3: the winner compacts past the token.
        let (_, out) = round(&store, a).await;
        let compacted = out.unwrap().expect("a wins interval 3");
        assert!(compacted > token_rev, "compacted to {compacted}, token at {token_rev}");
        let (_, out) = round(&store, b).await;
        assert_eq!(out.unwrap(), None, "b lost interval 3");
    }

    #[test]
    fn intervals_parse_as_go_durations() {
        assert_eq!(parse_interval("5m0s"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_interval("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_interval("1h30m"), Ok(Duration::from_secs(5400)));
        assert_eq!(parse_interval("1500ms"), Ok(Duration::from_millis(1500)));
        assert_eq!(parse_interval("0"), Ok(Duration::ZERO));
        assert_eq!(parse_interval("0s"), Ok(Duration::ZERO));
        for bad in ["", "5", "m", "5x", "-5m", "5 m"] {
            assert!(parse_interval(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_store_without_claims_compacts_nothing_and_says_so() {
        let store = crate::test_store::MemStore::default();
        let (next, out) = round(&store, Round::default()).await;
        assert!(out.is_err());
        assert_eq!(next, Round::default());
    }
}
