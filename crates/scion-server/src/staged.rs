//! Derived data of a batch of changesets, staged in memory until it is stored.
//!
//! Deriving a changeset reads what its parent's derivation just wrote: trees,
//! path-history nodes and derived pointers. [`Staged`] answers those reads
//! from memory and holds the writes, so a batch derives without waiting on
//! the stores. It also answers for the batch's own changesets, so derivation
//! need not wait for their rows to be inserted. [`Staged::store_objects`]
//! writes every object concurrently and [`Staged::store_pointers`], called
//! once the objects and the changeset rows are stored, every derived pointer
//! in one call. A pointer never names a missing object or changeset, so an
//! interrupted batch leaves only changesets whose derived data is computed
//! again on first use.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use scion_store::{
    ChangeRequest, DerivedPointer, LandOutcome, MetadataStore, ObjectStore, RefState, Result,
};
use scion_types::{BlobId, Changeset, ChangesetId, Tree, TreeId};

/// Most object writes in flight at once when a batch is stored.
const STORE_CONCURRENCY: usize = 64;

/// Objects and derived pointers written by one batch, over the stores they
/// are read from and finally written to. Every other call goes straight to
/// the stores.
pub(crate) struct Staged<'a> {
    meta: &'a dyn MetadataStore,
    objects: &'a dyn ObjectStore,
    repo: &'a str,
    batch: HashMap<ChangesetId, &'a Changeset>,
    writes: Mutex<Writes>,
}

#[derive(Default)]
struct Writes {
    trees: HashMap<TreeId, Tree>,
    blobs: HashMap<BlobId, Bytes>,
    derived: HashMap<(String, u32, ChangesetId), Vec<u8>>,
}

impl<'a> Staged<'a> {
    /// Stage derived data of `batch`, changesets of `repo`, over `meta` and
    /// `objects`.
    pub(crate) fn new(
        repo: &'a str,
        meta: &'a dyn MetadataStore,
        objects: &'a dyn ObjectStore,
        batch: &'a [Changeset],
    ) -> Self {
        Self {
            meta,
            objects,
            repo,
            batch: batch.iter().map(|cs| (cs.id(), cs)).collect(),
            writes: Mutex::default(),
        }
    }

    fn writes(&self) -> std::sync::MutexGuard<'_, Writes> {
        self.writes.lock().expect("poisoned")
    }

    /// Store the objects staged so far.
    pub(crate) async fn store_objects(&self) -> Result<()> {
        let (trees, blobs) = {
            let mut writes = self.writes();
            (
                std::mem::take(&mut writes.trees),
                std::mem::take(&mut writes.blobs),
            )
        };
        let objects = trees
            .into_values()
            .map(Object::Tree)
            .chain(blobs.into_values().map(Object::Blob));
        stream::iter(objects)
            .map(|object| put(self.objects, object))
            .buffer_unordered(STORE_CONCURRENCY)
            .try_collect()
            .await
    }

    /// Store the staged derived pointers. Called after
    /// [`store_objects`](Self::store_objects) and the batch's insert.
    pub(crate) async fn store_pointers(self) -> Result<()> {
        let Writes { derived, .. } = self.writes.into_inner().expect("poisoned");
        let pointers = derived
            .into_iter()
            .map(|((ty, version, cs), pointer)| (ty, version, cs, pointer))
            .collect();
        self.meta.put_derived_many(self.repo, pointers).await
    }
}

enum Object {
    Tree(Tree),
    Blob(Bytes),
}

async fn put(objects: &dyn ObjectStore, object: Object) -> Result<()> {
    match object {
        Object::Tree(tree) => objects.put_tree(&tree).await.map(drop),
        Object::Blob(blob) => objects.put_blob(blob).await.map(drop),
    }
}

#[async_trait]
impl ObjectStore for Staged<'_> {
    async fn put_blob(&self, bytes: Bytes) -> Result<BlobId> {
        let id = BlobId::hash(&bytes);
        self.writes().blobs.insert(id, bytes);
        Ok(id)
    }

    async fn get_blob(&self, id: BlobId) -> Result<Option<Bytes>> {
        let staged = self.writes().blobs.get(&id).cloned();
        match staged {
            Some(bytes) => Ok(Some(bytes)),
            None => self.objects.get_blob(id).await,
        }
    }

    async fn has_blobs(&self, ids: &[BlobId]) -> Result<Vec<bool>> {
        let mut present = self.objects.has_blobs(ids).await?;
        let writes = self.writes();
        for (id, present) in ids.iter().zip(&mut present) {
            *present |= writes.blobs.contains_key(id);
        }
        Ok(present)
    }

    async fn put_tree(&self, tree: &Tree) -> Result<TreeId> {
        let id = tree.id();
        self.writes()
            .trees
            .entry(id)
            .or_insert_with(|| tree.clone());
        Ok(id)
    }

    async fn get_tree(&self, id: TreeId) -> Result<Option<Tree>> {
        let staged = self.writes().trees.get(&id).cloned();
        match staged {
            Some(tree) => Ok(Some(tree)),
            None => self.objects.get_tree(id).await,
        }
    }
}

#[async_trait]
impl MetadataStore for Staged<'_> {
    async fn create_repo(&self, name: &str) -> Result<()> {
        self.meta.create_repo(name).await
    }

    async fn put_changesets(&self, batch: &[Changeset]) -> Result<()> {
        self.meta.put_changesets(batch).await
    }

    async fn get_changeset(&self, repo: &str, id: ChangesetId) -> Result<Option<Changeset>> {
        match self.batch.get(&id) {
            Some(cs) => Ok(Some((*cs).clone())),
            None => self.meta.get_changeset(repo, id).await,
        }
    }

    async fn generation(&self, repo: &str, id: ChangesetId) -> Result<Option<u64>> {
        self.meta.generation(repo, id).await
    }

    async fn create_ref(&self, repo: &str, name: &str, target: ChangesetId) -> Result<RefState> {
        self.meta.create_ref(repo, name, target).await
    }

    async fn get_ref(&self, repo: &str, name: &str) -> Result<Option<RefState>> {
        self.meta.get_ref(repo, name).await
    }

    async fn move_ref(
        &self,
        repo: &str,
        name: &str,
        expected_version: u64,
        new_target: ChangesetId,
    ) -> Result<RefState> {
        self.meta
            .move_ref(repo, name, expected_version, new_target)
            .await
    }

    async fn list_refs(&self, repo: &str) -> Result<Vec<(String, RefState)>> {
        self.meta.list_refs(repo).await
    }

    async fn put_derived_many(&self, repo: &str, pointers: Vec<DerivedPointer>) -> Result<()> {
        debug_assert_eq!(repo, self.repo, "a batch derives one repository");
        let mut writes = self.writes();
        for (ty, version, cs, pointer) in pointers {
            writes.derived.insert((ty, version, cs), pointer);
        }
        Ok(())
    }

    async fn get_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
    ) -> Result<Option<Vec<u8>>> {
        debug_assert_eq!(repo, self.repo, "a batch derives one repository");
        let staged = self
            .writes()
            .derived
            .get(&(ty.to_owned(), version, cs))
            .cloned();
        match staged {
            Some(pointer) => Ok(Some(pointer)),
            // Derived here even if the changeset was created before: the
            // derivation is deterministic and the pointers' insert keeps the
            // first value.
            None if self.batch.contains_key(&cs) => Ok(None),
            None => self.meta.get_derived(repo, ty, version, cs).await,
        }
    }

    async fn put_ingest_checkpoints(
        &self,
        repo: &str,
        done: &[(Vec<u8>, ChangesetId)],
    ) -> Result<()> {
        self.meta.put_ingest_checkpoints(repo, done).await
    }

    async fn ingest_checkpoints(&self, repo: &str) -> Result<Vec<(Vec<u8>, ChangesetId)>> {
        self.meta.ingest_checkpoints(repo).await
    }

    async fn submit_change_request(
        &self,
        repo: &str,
        head: ChangesetId,
        base: ChangesetId,
    ) -> Result<ChangeRequest> {
        self.meta.submit_change_request(repo, head, base).await
    }

    async fn get_change_request(&self, repo: &str, id: u64) -> Result<Option<ChangeRequest>> {
        self.meta.get_change_request(repo, id).await
    }

    async fn claim_next_change_request(&self, repo: &str) -> Result<Option<ChangeRequest>> {
        self.meta.claim_next_change_request(repo).await
    }

    async fn landing_change_request(&self, repo: &str) -> Result<Option<ChangeRequest>> {
        self.meta.landing_change_request(repo).await
    }

    async fn finish_change_request(
        &self,
        repo: &str,
        id: u64,
        outcome: LandOutcome,
    ) -> Result<ChangeRequest> {
        self.meta.finish_change_request(repo, id, outcome).await
    }

    async fn repos_with_open_change_requests(&self) -> Result<Vec<String>> {
        self.meta.repos_with_open_change_requests().await
    }
}
