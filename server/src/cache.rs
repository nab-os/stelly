//! Qobuz responses held in memory, so a page opened twice costs one request.
//!
//! Every browse read queues on the account's 2/s budget, the same one a running
//! crawl is spending, so a repeat visit to an album served from here is the
//! difference between instant and waiting behind the crawl. Nothing is kept
//! across restarts: the corpus is the durable copy, this is only the hot one.

use anyhow::{anyhow, Context, Result};
use std::any::Any;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

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

    pub fn clear(&self) {
        self.0.invalidate_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

        cache.clear();
        cache.get_or::<Vec<i32>, _>("a".into(), fetch()).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
