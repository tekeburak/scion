//! A bounded in-process memo of immutable values.
//!
//! Objects are content-addressed and every metadata row cached here
//! (changesets, derived pointers, repository ids) is write-once, so an entry
//! can never go stale: no invalidation exists or is needed. Refs are mutable
//! and are never cached.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;

/// At most `budget` bytes (as weighed by the caller) of recently read or
/// written values. When an insert would exceed the budget the memo starts
/// over, which keeps the bound without per-entry bookkeeping; the working set
/// of a derivation (the directories on the paths it changes) refills it at
/// once.
#[derive(Debug)]
pub(crate) struct Recent<K, V> {
    budget: usize,
    inner: Mutex<(HashMap<K, V>, usize)>,
}

impl<K: Eq + Hash, V: Clone> Recent<K, V> {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            budget,
            inner: Mutex::new((HashMap::new(), 0)),
        }
    }

    /// The value for `key`, if remembered. A hit spends the task's
    /// cooperative-scheduling budget like a ready I/O resource would, so a
    /// long derivation served from memory still yields to other requests.
    pub(crate) async fn get(&self, key: &K) -> Option<V> {
        let hit = self.inner.lock().expect("poisoned").0.get(key).cloned();
        if hit.is_some() {
            tokio::task::consume_budget().await;
        }
        hit
    }

    /// Remember `value`, which weighs `weight` bytes.
    pub(crate) fn insert(&self, key: K, value: V, weight: usize) {
        let mut inner = self.inner.lock().expect("poisoned");
        let (map, used) = &mut *inner;
        if *used + weight > self.budget {
            map.clear();
            *used = 0;
        }
        if map.insert(key, value).is_none() {
            *used += weight;
        }
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
            assert!(memo.inner.lock().unwrap().1 <= 100);
        }
        assert!(memo.inner.lock().unwrap().0.len() <= 10);
    }
}
