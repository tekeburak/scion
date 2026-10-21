//! The stores the simulated server runs on: a [`MemoryStore`] behind a
//! wrapper that yields to the scheduler before every call (so concurrent
//! requests and land workers interleave as the seed says), fails the calls
//! the simulation arms, and remembers the first value written under every
//! key so the run can check that nothing was overwritten.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use scion_store::{
    ChangeRequest, ChangeRequestStatus, DerivedPointer, Grant, LandOutcome, MemoryStore,
    MetadataStore, ObjectStore, RefState, Result, StoreError,
};
use scion_types::{BlobId, Changeset, ChangesetId, Tree, TreeId};

use crate::Rng;

/// A call the simulation can make fail once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Fault {
    /// The next `move_ref` of `main`: a land worker stopping after it
    /// created the rebased stack and before its compare-and-swap.
    MoveMain,
    /// The next `finish_change_request`: a land worker stopping after its
    /// compare-and-swap and before recording the outcome.
    Finish,
}

/// Every first write, by key.
#[derive(Default)]
pub(crate) struct Written {
    pub(crate) changesets: BTreeMap<ChangesetId, Changeset>,
    pub(crate) blobs: BTreeMap<BlobId, Bytes>,
    pub(crate) trees: BTreeMap<TreeId, Tree>,
    pub(crate) derived: BTreeMap<(String, String, u32, ChangesetId), Vec<u8>>,
    /// Writes that differed from the first one under their key.
    pub(crate) overwrites: Vec<String>,
}

pub(crate) struct SimStore {
    pub(crate) inner: MemoryStore,
    rng: Arc<Rng>,
    armed: Mutex<Vec<Fault>>,
    pub(crate) written: Mutex<Written>,
}

impl SimStore {
    pub(crate) fn new(rng: Arc<Rng>) -> Self {
        Self {
            inner: MemoryStore::new(),
            rng,
            armed: Mutex::new(Vec::new()),
            written: Mutex::new(Written::default()),
        }
    }

    pub(crate) fn arm(&self, fault: Fault) {
        self.armed.lock().expect("poisoned").push(fault);
    }

    pub(crate) fn disarm(&self) {
        self.armed.lock().expect("poisoned").clear();
    }

    /// Let other tasks run first, one call in three.
    async fn step(&self) {
        if self.rng.below(3) == 0 {
            tokio::task::yield_now().await;
        }
    }

    /// Fail with an injected error if `fault` is armed, disarming it.
    fn trip(&self, fault: Fault) -> Result<()> {
        let mut armed = self.armed.lock().expect("poisoned");
        match armed.iter().position(|f| *f == fault) {
            Some(i) => {
                armed.remove(i);
                Err(StoreError::Backend(format!("injected {fault:?}").into()))
            }
            None => Ok(()),
        }
    }

    /// Remember `value` under `key`, or note that it differs from the first.
    fn record<K: Ord + std::fmt::Debug, V: PartialEq>(
        overwrites: &mut Vec<String>,
        map: &mut BTreeMap<K, V>,
        key: K,
        value: V,
    ) {
        match map.get(&key) {
            Some(first) if *first != value => overwrites.push(format!("{key:?}")),
            Some(_) => {}
            None => {
                map.insert(key, value);
            }
        }
    }
}

#[async_trait]
impl MetadataStore for SimStore {
    async fn create_repo(&self, name: &str) -> Result<()> {
        self.step().await;
        self.inner.create_repo(name).await
    }

    async fn put_changesets(&self, batch: &[Changeset]) -> Result<()> {
        self.step().await;
        self.inner.put_changesets(batch).await?;
        let mut w = self.written.lock().expect("poisoned");
        let w = &mut *w;
        for cs in batch {
            Self::record(&mut w.overwrites, &mut w.changesets, cs.id(), cs.clone());
        }
        Ok(())
    }

    async fn get_changeset(&self, repo: &str, id: ChangesetId) -> Result<Option<Changeset>> {
        self.step().await;
        self.inner.get_changeset(repo, id).await
    }

    async fn generation(&self, repo: &str, id: ChangesetId) -> Result<Option<u64>> {
        self.step().await;
        self.inner.generation(repo, id).await
    }

    async fn create_ref(&self, repo: &str, name: &str, target: ChangesetId) -> Result<RefState> {
        self.step().await;
        self.inner.create_ref(repo, name, target).await
    }

    async fn get_ref(&self, repo: &str, name: &str) -> Result<Option<RefState>> {
        self.step().await;
        self.inner.get_ref(repo, name).await
    }

    async fn move_ref(
        &self,
        repo: &str,
        name: &str,
        expected_version: u64,
        new_target: ChangesetId,
    ) -> Result<RefState> {
        self.step().await;
        if name == "main" {
            self.trip(Fault::MoveMain)?;
        }
        self.inner
            .move_ref(repo, name, expected_version, new_target)
            .await
    }

    async fn list_refs(&self, repo: &str) -> Result<Vec<(String, RefState)>> {
        self.step().await;
        self.inner.list_refs(repo).await
    }

    async fn put_derived_many(&self, repo: &str, pointers: Vec<DerivedPointer>) -> Result<()> {
        self.step().await;
        self.inner.put_derived_many(repo, pointers.clone()).await?;
        let mut w = self.written.lock().expect("poisoned");
        let w = &mut *w;
        for (ty, version, cs, pointer) in pointers {
            let key = (repo.to_owned(), ty, version, cs);
            Self::record(&mut w.overwrites, &mut w.derived, key, pointer);
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
        self.step().await;
        self.inner.get_derived(repo, ty, version, cs).await
    }

    async fn put_ingest_checkpoints(
        &self,
        repo: &str,
        done: &[(Vec<u8>, ChangesetId)],
    ) -> Result<()> {
        self.step().await;
        self.inner.put_ingest_checkpoints(repo, done).await
    }

    async fn ingest_checkpoints(&self, repo: &str) -> Result<Vec<(Vec<u8>, ChangesetId)>> {
        self.step().await;
        self.inner.ingest_checkpoints(repo).await
    }

    async fn submit_change_request(
        &self,
        repo: &str,
        head: ChangesetId,
        base: ChangesetId,
        submitter: &str,
    ) -> Result<ChangeRequest> {
        self.step().await;
        self.inner
            .submit_change_request(repo, head, base, submitter)
            .await
    }

    async fn get_change_request(&self, repo: &str, id: u64) -> Result<Option<ChangeRequest>> {
        self.step().await;
        self.inner.get_change_request(repo, id).await
    }

    async fn list_change_requests(
        &self,
        repo: &str,
        status: Option<ChangeRequestStatus>,
        limit: u32,
    ) -> Result<Vec<ChangeRequest>> {
        self.step().await;
        self.inner.list_change_requests(repo, status, limit).await
    }

    async fn find_landings(
        &self,
        repo: &str,
        changesets: &[ChangesetId],
    ) -> Result<Vec<(ChangesetId, ChangeRequest)>> {
        self.step().await;
        self.inner.find_landings(repo, changesets).await
    }

    async fn claim_next_change_request(&self, repo: &str) -> Result<Option<ChangeRequest>> {
        self.step().await;
        self.inner.claim_next_change_request(repo).await
    }

    async fn landing_change_request(&self, repo: &str) -> Result<Option<ChangeRequest>> {
        self.step().await;
        self.inner.landing_change_request(repo).await
    }

    async fn finish_change_request(
        &self,
        repo: &str,
        id: u64,
        outcome: LandOutcome,
    ) -> Result<ChangeRequest> {
        self.step().await;
        self.trip(Fault::Finish)?;
        self.inner.finish_change_request(repo, id, outcome).await
    }

    async fn repos_with_open_change_requests(&self) -> Result<Vec<String>> {
        self.step().await;
        self.inner.repos_with_open_change_requests().await
    }

    async fn put_grant(&self, repo: &str, grant: &Grant) -> Result<()> {
        self.step().await;
        self.inner.put_grant(repo, grant).await
    }

    async fn delete_grant(&self, repo: &str, principal: &str, path_prefix: &str) -> Result<bool> {
        self.step().await;
        self.inner.delete_grant(repo, principal, path_prefix).await
    }

    async fn grants_for(&self, repo: &str, principal: &str) -> Result<Vec<Grant>> {
        self.step().await;
        self.inner.grants_for(repo, principal).await
    }

    async fn grants(&self, repo: &str) -> Result<Vec<Grant>> {
        self.step().await;
        self.inner.grants(repo).await
    }

    async fn repos_of(&self, principal: &str) -> Result<Vec<String>> {
        self.step().await;
        self.inner.repos_of(principal).await
    }
}

#[async_trait]
impl ObjectStore for SimStore {
    async fn put_blob(&self, bytes: Bytes) -> Result<BlobId> {
        self.step().await;
        let id = self.inner.put_blob(bytes.clone()).await?;
        let mut w = self.written.lock().expect("poisoned");
        let w = &mut *w;
        Self::record(&mut w.overwrites, &mut w.blobs, id, bytes);
        Ok(id)
    }

    async fn get_blob(&self, id: BlobId) -> Result<Option<Bytes>> {
        self.step().await;
        self.inner.get_blob(id).await
    }

    async fn has_blobs(&self, ids: &[BlobId]) -> Result<Vec<bool>> {
        self.step().await;
        self.inner.has_blobs(ids).await
    }

    async fn put_tree(&self, tree: &Tree) -> Result<TreeId> {
        self.step().await;
        let id = self.inner.put_tree(tree).await?;
        let mut w = self.written.lock().expect("poisoned");
        let w = &mut *w;
        Self::record(&mut w.overwrites, &mut w.trees, id, tree.clone());
        Ok(id)
    }

    async fn get_tree(&self, id: TreeId) -> Result<Option<Tree>> {
        self.step().await;
        self.inner.get_tree(id).await
    }
}
