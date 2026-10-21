//! The derivation framework.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use bytes::Bytes;
use scion_store::{MetadataStore, ObjectStore};
use scion_types::{BlobId, Changeset, ChangesetId, Tree, TreeId};

use crate::{DeriveError, Result};

/// A type of derived data.
///
/// The definition is recursive: the value for a changeset is computed from the
/// changeset itself and the values for its parents (in `parents` order). The
/// value is persisted in the metadata store as an opaque pointer keyed by
/// `(repo, NAME, VERSION, changeset)`; bumping `VERSION` invalidates every
/// stored value of the type.
#[async_trait]
pub trait Derivable: Clone + Send + Sync + 'static {
    /// Stable name used as the storage key.
    const NAME: &'static str;
    /// Bump when `derive` or the pointer format changes.
    const VERSION: u32;
    /// Whether `derive` reads its parents' values. A type that does not is
    /// derived for the requested changeset alone, with no parent values;
    /// its `derive` finds and derives whatever earlier values it needs.
    const READS_PARENTS: bool = true;

    /// Compute the value for `cs` from the values for its parents.
    async fn derive(ctx: &Deriver<'_>, cs: &Changeset, parents: &[Self]) -> Result<Self>;

    /// Serialise for the `derived` table.
    fn to_pointer(&self) -> Vec<u8>;

    /// Parse a pointer written by [`to_pointer`](Self::to_pointer).
    fn from_pointer(bytes: &[u8]) -> Option<Self>;
}

/// Most changesets one [`Deriver::derive`] holds in memory at once.
const MAX_LOADED: usize = 64;

/// Derives and caches [`Derivable`] values for one repository.
#[derive(Clone, Copy)]
pub struct Deriver<'a> {
    pub repo: &'a str,
    pub meta: &'a dyn MetadataStore,
    pub objects: &'a dyn ObjectStore,
}

impl std::fmt::Debug for Deriver<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deriver")
            .field("repo", &self.repo)
            .finish_non_exhaustive()
    }
}

impl<'a> Deriver<'a> {
    /// A deriver over the given stores.
    #[must_use]
    pub fn new(repo: &'a str, meta: &'a dyn MetadataStore, objects: &'a dyn ObjectStore) -> Self {
        Self {
            repo,
            meta,
            objects,
        }
    }

    /// The tree `id` of this repository, which must exist.
    pub async fn tree(&self, id: TreeId) -> Result<Tree> {
        self.meta
            .get_tree(self.repo, id)
            .await?
            .ok_or(DeriveError::MissingTree(id))
    }

    /// The path-history page `id` of this repository, which must exist.
    pub(crate) async fn page(&self, id: BlobId) -> Result<Bytes> {
        self.meta
            .get_pages(self.repo, &[id])
            .await?
            .pop()
            .flatten()
            .ok_or(DeriveError::MissingObject(id))
    }

    /// Read a previously derived value without deriving.
    pub async fn fetch<D: Derivable>(&self, cs: ChangesetId) -> Result<Option<D>> {
        match self
            .meta
            .get_derived(self.repo, D::NAME, D::VERSION, cs)
            .await?
        {
            None => Ok(None),
            Some(bytes) => D::from_pointer(&bytes)
                .map(Some)
                .ok_or(DeriveError::CorruptPointer { ty: D::NAME, cs }),
        }
    }

    /// [`fetch`](Self::fetch) for each of `ids`, in one metadata read.
    pub async fn fetch_many<D: Derivable>(&self, ids: &[ChangesetId]) -> Result<Vec<Option<D>>> {
        let pointers = self
            .meta
            .get_derived_many(self.repo, D::NAME, D::VERSION, ids)
            .await?;
        ids.iter()
            .zip(pointers)
            .map(|(cs, pointer)| match pointer {
                None => Ok(None),
                Some(bytes) => {
                    D::from_pointer(&bytes)
                        .map(Some)
                        .ok_or(DeriveError::CorruptPointer {
                            ty: D::NAME,
                            cs: *cs,
                        })
                }
            })
            .collect()
    }

    /// Return the value for `target`, deriving it and any underived ancestors.
    ///
    /// Ancestors are derived parents before children; the walk stops at
    /// changesets whose value is already stored, so its cost is bounded by
    /// the number of underived ancestors, not by the length of the history.
    /// Memory follows the frontier, not the history: every underived
    /// ancestor is held as its id only, at most 64 changesets
    /// are loaded at once, and a value is dropped as soon as every child that
    /// reads it is derived.
    pub async fn derive<D: Derivable>(&self, target: ChangesetId) -> Result<D> {
        if let Some(v) = self.fetch::<D>(target).await? {
            return Ok(v);
        }
        if D::READS_PARENTS {
            return Ok(self.derive_walk(target).await?.0);
        }
        let cs = self
            .meta
            .get_changeset(self.repo, target)
            .await?
            .ok_or(DeriveError::MissingChangeset(target))?;
        let value = D::derive(self, &cs, &[]).await?;
        self.meta
            .put_derived(self.repo, D::NAME, D::VERSION, target, value.to_pointer())
            .await?;
        Ok(value)
    }

    /// Derive `target`, whose value is not stored, and its underived
    /// ancestors. Also returns the most changesets and values held at once.
    async fn derive_walk<D: Derivable>(&self, target: ChangesetId) -> Result<(D, usize)> {
        // Find the underived ancestors in post-order (parents first). The
        // first `MAX_LOADED` changesets read stay loaded, which covers the
        // common case of a few new changesets over a derived history in one
        // read each; the rest are read again in batches when their turn comes.
        let mut visited: HashSet<ChangesetId> = HashSet::new();
        // Children in this walk that have yet to read a changeset's value.
        let mut readers: HashMap<ChangesetId, usize> = HashMap::new();
        let mut memo: HashMap<ChangesetId, D> = HashMap::new();
        let mut loaded: HashMap<ChangesetId, Changeset> = HashMap::new();
        let mut order = Vec::new();
        let mut peak = 0;
        // (changeset, expanded): the second pop of an id happens after all its
        // underived parents have been ordered.
        let mut stack: Vec<(ChangesetId, bool)> = vec![(target, false)];
        while let Some((id, expanded)) = stack.pop() {
            if expanded {
                order.push(id);
                continue;
            }
            if !visited.insert(id) {
                continue;
            }
            let cs = self
                .meta
                .get_changeset(self.repo, id)
                .await?
                .ok_or(DeriveError::MissingChangeset(id))?;
            let stored = if cs.parents.is_empty() {
                Vec::new()
            } else {
                self.fetch_many::<D>(&cs.parents).await?
            };
            stack.push((id, true));
            for (p, value) in cs.parents.iter().zip(stored) {
                *readers.entry(*p).or_default() += 1;
                match value {
                    Some(v) => {
                        memo.insert(*p, v);
                    }
                    None if !visited.contains(p) => stack.push((*p, false)),
                    None => {}
                }
            }
            if loaded.len() < MAX_LOADED {
                loaded.insert(id, cs);
            }
            peak = peak.max(loaded.len() + memo.len());
        }
        drop(visited);

        for chunk in order.chunks(MAX_LOADED) {
            let missing: Vec<ChangesetId> = chunk
                .iter()
                .filter(|id| !loaded.contains_key(id))
                .copied()
                .collect();
            if !missing.is_empty() {
                let read = self.meta.get_changesets(self.repo, &missing).await?;
                for (id, cs) in missing.into_iter().zip(read) {
                    loaded.insert(id, cs.ok_or(DeriveError::MissingChangeset(id))?);
                }
            }
            peak = peak.max(loaded.len() + memo.len());
            for id in chunk {
                let cs = loaded
                    .remove(id)
                    .ok_or(DeriveError::MissingChangeset(*id))?;
                let parents = cs
                    .parents
                    .iter()
                    .map(|p| {
                        memo.get(p)
                            .cloned()
                            .ok_or(DeriveError::MissingChangeset(*p))
                    })
                    .collect::<Result<Vec<D>>>()?;
                let value = D::derive(self, &cs, &parents).await?;
                self.meta
                    .put_derived(self.repo, D::NAME, D::VERSION, *id, value.to_pointer())
                    .await?;
                for p in &cs.parents {
                    if let Some(n) = readers.get_mut(p) {
                        *n -= 1;
                        if *n == 0 {
                            readers.remove(p);
                            memo.remove(p);
                        }
                    }
                }
                if readers.contains_key(id) || *id == target {
                    memo.insert(*id, value);
                }
            }
        }

        let value = memo
            .remove(&target)
            .ok_or(DeriveError::MissingChangeset(target))?;
        Ok((value, peak))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use scion_store::{MemoryStore, MetadataStore};
    use scion_types::RepoPath;

    use super::*;
    use crate::test_util::{CountingMeta, REPO, changeset, tracked};
    use crate::{PathHistory, RootTree, SkipList};

    /// The shape of the importer's large-history fixture: a linear history
    /// where changeset `i` rewrites one of 500 files in 20 directories.
    async fn long_history(store: &MemoryStore, n: usize) -> Vec<ChangesetId> {
        let mut ids: Vec<ChangesetId> = Vec::with_capacity(n);
        for i in 1..=n {
            let path = RepoPath::new(format!("dir{}/file{}.txt", i % 20, i % 500)).unwrap();
            let body = format!("line one\nversion {i}\nline three\n");
            let changes = BTreeMap::from([(path, tracked(body.as_bytes()))]);
            let cs = changeset(
                ids.last().copied().into_iter().collect(),
                changes,
                &i.to_string(),
            );
            ids.push(store.put_changeset(&cs).await.unwrap());
        }
        ids
    }

    #[tokio::test]
    async fn cold_derivation_holds_a_constant_number_of_changesets() {
        const N: usize = 20_000;
        let store = MemoryStore::with_repo(REPO);
        let ids = long_history(&store, N).await;
        let tip = ids[N - 1];
        let counting = CountingMeta::new(&store);
        let d = Deriver::new(REPO, &counting, &store);

        let (_, peak) = d.derive_walk::<RootTree>(tip).await.unwrap();
        assert!(
            peak <= 2 * MAX_LOADED + 1,
            "held {peak} changesets and values"
        );
        // One read to find each changeset's parents, then one per batch.
        let reads = counting.changeset_reads();
        assert!(reads <= 2 * N, "{reads} changeset reads for {N} changesets");

        let (_, peak) = d.derive_walk::<SkipList>(tip).await.unwrap();
        assert!(
            peak <= 2 * MAX_LOADED + 1,
            "held {peak} changesets and values"
        );
        let (_, peak) = d.derive_walk::<PathHistory>(tip).await.unwrap();
        assert!(
            peak <= 2 * MAX_LOADED + 1,
            "held {peak} changesets and values"
        );

        // Every changeset was derived, in an order that let each one read
        // its parent's value.
        let gens = d.fetch_many::<SkipList>(&ids).await.unwrap();
        for (i, g) in gens.into_iter().enumerate() {
            assert_eq!(g.unwrap().generation, i as u64);
        }
    }

    #[tokio::test]
    async fn values_are_kept_until_every_child_has_read_them() {
        // root ← a ← c, root ← b ← c (a diamond), then d ← c: `root` is
        // read by two children, `c` by one.
        let store = MemoryStore::with_repo(REPO);
        let node = |parents: Vec<ChangesetId>, tag: &str| changeset(parents, BTreeMap::new(), tag);
        let root = store.put_changeset(&node(vec![], "root")).await.unwrap();
        let a = store.put_changeset(&node(vec![root], "a")).await.unwrap();
        let b = store.put_changeset(&node(vec![root], "b")).await.unwrap();
        let c = store.put_changeset(&node(vec![a, b], "c")).await.unwrap();
        let tip = store.put_changeset(&node(vec![c], "d")).await.unwrap();
        let d = Deriver::new(REPO, &store, &store);
        let top = d.derive::<SkipList>(tip).await.unwrap();
        assert_eq!(top.generation, 3);
        assert_eq!(d.fetch::<SkipList>(b).await.unwrap().unwrap().generation, 1);
    }
}
