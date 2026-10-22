//! A bounded in-process memo of immutable values.
//!
//! Objects are content-addressed and most metadata rows cached here
//! (changesets, derived pointers, repository ids) are write-once, so their
//! entries can never go stale. The memos of refs and grants are the
//! exception: the Postgres store drops their entries when a write announces
//! itself (see [`PostgresStore::listen`](crate::PostgresStore::listen)).

use std::hash::Hash;

/// At most `budget` bytes (as weighed by the caller) of recently read or
/// written values, evicted least recently used first (`moka`), so a hot
/// working set survives a scan that would overflow the budget.
pub(crate) struct Recent<K, V> {
    inner: moka::sync::Cache<K, (V, u32)>,
}

impl<K, V> std::fmt::Debug for Recent<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recent").finish_non_exhaustive()
    }
}

impl<K, V> Recent<K, V>
where
    K: Eq + Hash + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            inner: moka::sync::Cache::builder()
                .max_capacity(budget as u64)
                .eviction_policy(moka::policy::EvictionPolicy::lru())
                .weigher(|_, (_, weight): &(V, u32)| *weight)
                .build(),
        }
    }

    /// The value for `key`, if remembered. A hit spends the task's
    /// cooperative-scheduling budget like a ready I/O resource would, so a
    /// long derivation served from memory still yields to other requests.
    pub(crate) async fn get(&self, key: &K) -> Option<V> {
        let hit = self.inner.get(key).map(|(v, _)| v);
        if hit.is_some() {
            tokio::task::consume_budget().await;
        }
        hit
    }

    /// Remember `value`, which weighs `weight` bytes.
    pub(crate) fn insert(&self, key: K, value: V, weight: usize) {
        let weight = u32::try_from(weight).unwrap_or(u32::MAX);
        self.inner.insert(key, (value, weight));
    }

    /// Forget `key`.
    pub(crate) fn remove(&self, key: &K) {
        self.inner.invalidate(key);
    }

    /// Forget everything.
    pub(crate) fn clear(&self) {
        self.inner.invalidate_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stays_within_budget() {
        let memo = Recent::new(100);
        for i in 0..1000u32 {
            memo.insert(i, i, 10);
            assert_eq!(memo.get(&i).await, Some(i));
            memo.inner.run_pending_tasks();
            assert!(memo.inner.weighted_size() <= 100);
        }
        assert!(memo.inner.entry_count() <= 10);
    }
}
