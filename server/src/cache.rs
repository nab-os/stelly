//! Qobuz responses held in memory, so a page opened twice costs one request.
//!
//! Every browse read queues on the account's 2/s budget, the same one a running
//! crawl is spending, so a repeat visit to an album served from here is the
//! difference between instant and waiting behind the crawl. Nothing is kept
//! across restarts: the corpus is the durable copy, this is only the hot one.

use anyhow::{anyhow, Context, Result};
use std::any::Any;
use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Entry = Arc<dyn Any + Send + Sync>;

/// One lifetime for everything in it. Entries of different types share a
/// cache, told apart by the endpoint name that starts every key.
pub struct Cache(moka::future::Cache<String, Entry>);

impl Cache {
    pub fn new(ttl: Duration, capacity: u64) -> Self {
        Self(
            moka::future::Cache::builder()
                .time_to_live(ttl)
                .max_capacity(capacity)
                .build(),
        )
    }

    /// The cached value, or `fetch`'s. Callers asking for the same key at the
    /// same time share one fetch, and a failed fetch is not remembered.
    pub async fn get_or<T, F>(&self, key: String, fetch: F) -> Result<T>
    where
        T: Clone + Send + Sync + 'static,
        F: Future<Output = Result<T>>,
    {
        let entry = self
            .0
            .try_get_with(key.clone(), async { fetch.await.map(|v| Arc::new(v) as Entry) })
            .await
            .map_err(|err| anyhow!("{err:#}"))?;
        entry
            .downcast_ref::<T>()
            .cloned()
            .with_context(|| format!("cache entry {key:?} holds another type"))
    }

    /// Replace whatever is held for `key`, for a refresh that should not wait
    /// for the old value to expire.
    pub async fn put<T: Send + Sync + 'static>(&self, key: String, value: T) {
        self.0.insert(key, Arc::new(value) as Entry).await;
    }
}

/// A cache that never makes anyone wait for an entry it holds: past
/// `fresh`, the entry is served as it is and fetched again in the
/// background, once however many ask meanwhile. Only past `max_age`, or
/// cleared, is it a miss, waited on like any other.
#[derive(Clone)]
pub struct Revalidating {
    held: moka::future::Cache<String, (Instant, Entry)>,
    fresh: Duration,
    /// Keys being fetched again in the background.
    refreshing: Arc<Mutex<HashSet<String>>>,
    /// Moved by `clear`, under the lock a refresh puts its answer in by: an
    /// answer asked for before a clear is not kept after it.
    cleared: Arc<AtomicU64>,
    putting: Arc<tokio::sync::Mutex<()>>,
}

impl Revalidating {
    pub fn new(fresh: Duration, max_age: Duration, capacity: u64) -> Self {
        Self {
            held: moka::future::Cache::builder().time_to_live(max_age).max_capacity(capacity).build(),
            fresh,
            refreshing: Arc::default(),
            cleared: Arc::default(),
            putting: Arc::default(),
        }
    }

    /// The held value, or `fetch`'s. Callers missing the same key at the
    /// same time share one fetch, and a failed fetch is not remembered; a
    /// failed refresh leaves the held value until it reaches `max_age`.
    pub async fn get_or<T, F>(&self, key: String, fetch: F) -> Result<T>
    where
        T: Clone + Send + Sync + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let entry = match self.held.get(&key).await {
            Some((at, entry)) => {
                if at.elapsed() >= self.fresh {
                    self.refresh(key.clone(), fetch);
                }
                entry
            }
            None => {
                self.held
                    .try_get_with(key.clone(), async { fetch.await.map(|v| (Instant::now(), Arc::new(v) as Entry)) })
                    .await
                    .map_err(|err| anyhow!("{err:#}"))?
                    .1
            }
        };
        entry
            .downcast_ref::<T>()
            .cloned()
            .with_context(|| format!("cache entry {key:?} holds another type"))
    }

    /// Fetch `key` again in the background, unless that is under way.
    fn refresh<T, F>(&self, key: String, fetch: F)
    where
        T: Send + Sync + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        if !self.refreshing.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone()) {
            return;
        }
        let (this, cleared) = (self.clone(), self.cleared.load(Ordering::SeqCst));
        tokio::spawn(async move {
            match fetch.await {
                Ok(value) => {
                    let _putting = this.putting.lock().await;
                    if this.cleared.load(Ordering::SeqCst) == cleared {
                        this.held.insert(key.clone(), (Instant::now(), Arc::new(value) as Entry)).await;
                    }
                }
                Err(err) => eprintln!("refreshing {key:?} failed, the one held is kept: {err:#}"),
            }
            this.refreshing.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        });
    }

    pub async fn clear(&self) {
        let _putting = self.putting.lock().await;
        self.cleared.fetch_add(1, Ordering::SeqCst);
        self.held.invalidate_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn hits_skip_the_fetch_and_errors_are_not_kept() {
        let cache = Cache::new(Duration::from_secs(60), 10);
        let calls = AtomicUsize::new(0);
        let fetch = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![1, 2, 3])
        };

        assert_eq!(cache.get_or("a".into(), fetch()).await.unwrap(), vec![1, 2, 3]);
        assert_eq!(cache.get_or("a".into(), fetch()).await.unwrap(), vec![1, 2, 3]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let failed: Result<Vec<i32>> = cache.get_or("b".into(), async { anyhow::bail!("down") }).await;
        assert!(failed.unwrap_err().to_string().contains("down"));
        assert_eq!(cache.get_or("b".into(), fetch()).await.unwrap(), vec![1, 2, 3]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Past its freshness an entry is served at once and fetched again
    /// once, behind it; a clear drops it, and a refresh that was under way
    /// does not bring it back.
    #[tokio::test]
    async fn stale_entries_are_served_while_fetched_again() {
        let cache = Revalidating::new(Duration::from_millis(100), Duration::from_secs(60), 10);
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = |wait: u64| {
            let calls = calls.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                Ok(calls.fetch_add(1, Ordering::SeqCst))
            }
        };

        assert_eq!(cache.get_or("a".into(), fetch(0)).await.unwrap(), 0);
        tokio::time::sleep(Duration::from_millis(150)).await;
        for _ in 0..5 {
            assert_eq!(cache.get_or("a".into(), fetch(50)).await.unwrap(), 0, "the stale one, at once");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(cache.get_or("a".into(), fetch(0)).await.unwrap(), 1, "refreshed once");
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(cache.get_or("a".into(), fetch(100)).await.unwrap(), 1);
        cache.clear().await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(cache.get_or("a".into(), fetch(0)).await.unwrap(), 3, "a miss, not the refresh from before the clear");
    }
}
