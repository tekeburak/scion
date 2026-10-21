//! Derived data of a batch of changesets, staged in memory until it is stored.
//!
//! Deriving a changeset reads what its parent's derivation just wrote: trees,
//! path-history nodes and derived pointers. [`Staged`] answers those reads
//! from memory and holds the writes, so a batch derives without waiting on
//! the stores. It also answers for the batch's own changesets, so derivation
//! need not wait for their rows to be inserted. [`Staged::store_objects`]
//! writes the batch's trees and pages in one statement each, alongside its
//! derived blobs, and [`Staged::store_pointers`], called
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
    ChangeRequest, ChangeRequestStatus, DerivedPointer, Grant, LandOutcome, MetadataStore,
    ObjectStore, RefState, Result,
};
use scion_types::{BlobId, Changeset, ChangesetId, Tree, TreeId};
use tokio::try_join;

/// Most blob writes in flight at once when a batch is stored.
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
    pages: HashMap<BlobId, Bytes>,
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
        let (trees, pages, blobs) = {
            let mut writes = self.writes();
            (
                std::mem::take(&mut writes.trees),
                std::mem::take(&mut writes.pages),
                std::mem::take(&mut writes.blobs),
            )
        };
        let trees: Vec<Tree> = trees.into_values().collect();
        let pages: Vec<Bytes> = pages.into_values().collect();
        let blobs = stream::iter(blobs.into_values())
            .map(|blob| self.objects.put_blob(blob))
            .buffer_unordered(STORE_CONCURRENCY)
            .try_collect::<Vec<BlobId>>();
        try_join!(
            self.meta.put_trees(self.repo, &trees),
            self.meta.put_pages(self.repo, &pages),
            blobs,
        )?;
        Ok(())
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
}

#[async_trait]
impl MetadataStore for Staged<'_> {
    async fn create_repo(&self, name: &str) -> Result<()> {
        self.meta.create_repo(name).await
    }

    async fn put_changesets(&self, batch: &[Changeset]) -> Result<()> {
        self.meta.put_changesets(batch).await
    }

    async fn put_trees(&self, repo: &str, trees: &[Tree]) -> Result<()> {
        debug_assert_eq!(repo, self.repo, "a batch derives one repository");
        let mut writes = self.writes();
        for tree in trees {
            writes
                .trees
                .entry(tree.id())
                .or_insert_with(|| tree.clone());
        }
        Ok(())
    }

    async fn get_trees(&self, repo: &str, ids: &[TreeId]) -> Result<Vec<Option<Tree>>> {
        debug_assert_eq!(repo, self.repo, "a batch derives one repository");
        let mut out: Vec<Option<Tree>> = {
            let writes = self.writes();
            ids.iter().map(|id| writes.trees.get(id).cloned()).collect()
        };
        let missing: Vec<TreeId> = ids
            .iter()
            .zip(&out)
            .filter(|(_, tree)| tree.is_none())
            .map(|(id, _)| *id)
            .collect();
        if !missing.is_empty() {
            let mut read = self.meta.get_trees(repo, &missing).await?.into_iter();
            for slot in out.iter_mut().filter(|slot| slot.is_none()) {
                *slot = read.next().flatten();
            }
        }
        Ok(out)
    }

    async fn put_pages(&self, repo: &str, pages: &[Bytes]) -> Result<()> {
        debug_assert_eq!(repo, self.repo, "a batch derives one repository");
        let mut writes = self.writes();
        for page in pages {
            writes.pages.insert(BlobId::hash(page), page.clone());
        }
        Ok(())
    }

    async fn get_pages(&self, repo: &str, ids: &[BlobId]) -> Result<Vec<Option<Bytes>>> {
        debug_assert_eq!(repo, self.repo, "a batch derives one repository");
        let mut out: Vec<Option<Bytes>> = {
            let writes = self.writes();
            ids.iter().map(|id| writes.pages.get(id).cloned()).collect()
        };
        let missing: Vec<BlobId> = ids
            .iter()
            .zip(&out)
            .filter(|(_, page)| page.is_none())
            .map(|(id, _)| *id)
            .collect();
        if !missing.is_empty() {
            let mut read = self.meta.get_pages(repo, &missing).await?.into_iter();
            for slot in out.iter_mut().filter(|slot| slot.is_none()) {
                *slot = read.next().flatten();
            }
        }
        Ok(out)
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
        submitter: &str,
    ) -> Result<ChangeRequest> {
        self.meta
            .submit_change_request(repo, head, base, submitter)
            .await
    }

    async fn get_change_request(&self, repo: &str, id: u64) -> Result<Option<ChangeRequest>> {
        self.meta.get_change_request(repo, id).await
    }

    async fn list_change_requests(
        &self,
        repo: &str,
        status: Option<ChangeRequestStatus>,
        limit: u32,
    ) -> Result<Vec<ChangeRequest>> {
        self.meta.list_change_requests(repo, status, limit).await
    }

    async fn find_landings(
        &self,
        repo: &str,
        changesets: &[ChangesetId],
    ) -> Result<Vec<(ChangesetId, ChangeRequest)>> {
        self.meta.find_landings(repo, changesets).await
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

    async fn reject_change_request(&self, repo: &str, id: u64) -> Result<ChangeRequest> {
        self.meta.reject_change_request(repo, id).await
    }

    async fn repos_with_open_change_requests(&self) -> Result<Vec<String>> {
        self.meta.repos_with_open_change_requests().await
    }

    async fn put_grant(&self, repo: &str, grant: &Grant) -> Result<()> {
        self.meta.put_grant(repo, grant).await
    }

    async fn delete_grant(&self, repo: &str, principal: &str, path_prefix: &str) -> Result<bool> {
        self.meta.delete_grant(repo, principal, path_prefix).await
    }

    async fn grants_for(&self, repo: &str, principal: &str) -> Result<Vec<Grant>> {
        self.meta.grants_for(repo, principal).await
    }

    async fn grants(&self, repo: &str) -> Result<Vec<Grant>> {
        self.meta.grants(repo).await
    }

    async fn repos_of(&self, principal: &str) -> Result<Vec<String>> {
        self.meta.repos_of(principal).await
    }
}
