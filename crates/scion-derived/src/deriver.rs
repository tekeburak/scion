//! The derivation framework.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use scion_store::{MetadataStore, ObjectStore};
use scion_types::{Changeset, ChangesetId};

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

    /// Return the value for `target`, deriving it and any underived ancestors.
    ///
    /// Ancestors are visited in post-order so every parent is derived before
    /// its children; the walk stops at changesets whose value is already
    /// stored, so its cost is bounded by the number of underived ancestors,
    /// not by the length of the history.
    pub async fn derive<D: Derivable>(&self, target: ChangesetId) -> Result<D> {
        if let Some(v) = self.fetch::<D>(target).await? {
            return Ok(v);
        }
        if !D::READS_PARENTS {
            let cs = self
                .meta
                .get_changeset(self.repo, target)
                .await?
                .ok_or(DeriveError::MissingChangeset(target))?;
            let value = D::derive(self, &cs, &[]).await?;
            self.meta
                .put_derived(self.repo, D::NAME, D::VERSION, target, value.to_pointer())
                .await?;
            return Ok(value);
        }

        let mut memo: HashMap<ChangesetId, D> = HashMap::new();
        let mut loaded: HashMap<ChangesetId, Changeset> = HashMap::new();
        let mut visited: HashSet<ChangesetId> = HashSet::new();
        // (changeset, expanded): the second pop of an id happens after all its
        // underived parents have been derived.
        let mut stack: Vec<(ChangesetId, bool)> = vec![(target, false)];

        while let Some((id, expanded)) = stack.pop() {
            if expanded {
                let cs = &loaded[&id];
                let mut parents = Vec::with_capacity(cs.parents.len());
                for p in &cs.parents {
                    parents.push(self.value_of::<D>(*p, &memo).await?);
                }
                let value = D::derive(self, cs, &parents).await?;
                self.meta
                    .put_derived(self.repo, D::NAME, D::VERSION, id, value.to_pointer())
                    .await?;
                memo.insert(id, value);
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
            stack.push((id, true));
            for p in &cs.parents {
                if !visited.contains(p) && self.fetch::<D>(*p).await?.is_none() {
                    stack.push((*p, false));
                }
            }
            loaded.insert(id, cs);
        }

        memo.remove(&target)
            .ok_or(DeriveError::MissingChangeset(target))
    }

    async fn value_of<D: Derivable>(
        &self,
        cs: ChangesetId,
        memo: &HashMap<ChangesetId, D>,
    ) -> Result<D> {
        if let Some(v) = memo.get(&cs) {
            return Ok(v.clone());
        }
        self.fetch::<D>(cs)
            .await?
            .ok_or(DeriveError::MissingChangeset(cs))
    }
}
