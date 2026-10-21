//! Shared test helpers: naive materialisation, changeset builders, counting
//! stores, and proptest generators.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use proptest::prelude::*;
use scion_store::{
    ChangeRequest, ChangeRequestStatus, DerivedPointer, Grant, LandOutcome, MemoryStore,
    MetadataStore, ObjectStore, RefState,
};
use scion_types::{
    BlobId, Changeset, ChangesetId, FileChange, FileType, Identity, RepoPath, Timestamp, Tree,
    TreeEntry, TreeId,
};

pub const REPO: &str = "t";

/// `path -> file`: the naive, fully materialised view of a tree.
pub type Flat = BTreeMap<RepoPath, (BlobId, FileType, u64)>;

/// Flatten a tree of [`REPO`] into `path -> file`.
pub async fn materialize(meta: &dyn MetadataStore, root: TreeId) -> Flat {
    let mut out = Flat::new();
    let mut stack = vec![(None::<RepoPath>, root)];
    while let Some((prefix, id)) = stack.pop() {
        let tree = meta.get_tree(REPO, id).await.unwrap().expect("tree exists");
        for (name, entry) in tree.entries {
            let path = match &prefix {
                Some(p) => p.join(&name),
                None => RepoPath::new(name.as_str()).unwrap(),
            };
            match entry {
                TreeEntry::Blob { id, kind, size } => {
                    out.insert(path, (id, kind, size));
                }
                TreeEntry::Tree { id } => stack.push((Some(path), id)),
            }
        }
    }
    out
}

pub fn changeset(
    parents: Vec<ChangesetId>,
    changes: BTreeMap<RepoPath, FileChange>,
    msg: &str,
) -> Changeset {
    Changeset {
        repo: REPO.into(),
        parents,
        author: Identity::new("a", "a@x"),
        author_date: Timestamp::new(0, 0),
        committer: None,
        committer_date: None,
        message: msg.into(),
        file_changes: changes,
        extra: BTreeMap::new(),
    }
}

pub fn tracked(content: &[u8]) -> FileChange {
    FileChange::Tracked {
        content: BlobId::hash(content),
        kind: FileType::Regular,
        size: u64::try_from(content.len()).unwrap(),
        copy_from: None,
    }
}

/// Counts blob reads and writes.
#[derive(Debug)]
pub struct Counting<'a> {
    pub inner: &'a MemoryStore,
    pub blob_puts: AtomicUsize,
    pub blob_gets: AtomicUsize,
}

impl<'a> Counting<'a> {
    pub fn new(inner: &'a MemoryStore) -> Self {
        Self {
            inner,
            blob_puts: AtomicUsize::new(0),
            blob_gets: AtomicUsize::new(0),
        }
    }
    pub fn blob_puts(&self) -> usize {
        self.blob_puts.load(Ordering::Relaxed)
    }
    pub fn blob_gets(&self) -> usize {
        self.blob_gets.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ObjectStore for Counting<'_> {
    async fn put_blob(&self, bytes: Bytes) -> scion_store::Result<BlobId> {
        self.blob_puts.fetch_add(1, Ordering::Relaxed);
        self.inner.put_blob(bytes).await
    }
    async fn get_blob(&self, id: BlobId) -> scion_store::Result<Option<Bytes>> {
        self.blob_gets.fetch_add(1, Ordering::Relaxed);
        self.inner.get_blob(id).await
    }
    async fn has_blobs(&self, ids: &[BlobId]) -> scion_store::Result<Vec<bool>> {
        self.inner.has_blobs(ids).await
    }
}

/// Small alphabet so paths collide and nest (file/dir replacement happens).
pub fn arb_path() -> impl Strategy<Value = RepoPath> {
    prop::collection::vec("[ab][0-2]", 1..4).prop_map(|cs| RepoPath::new(cs.join("/")).unwrap())
}

pub fn arb_change() -> impl Strategy<Value = FileChange> {
    prop_oneof![
        2 => any::<u8>().prop_map(|b| tracked(&[b])),
        1 => Just(FileChange::Deleted),
    ]
}

pub fn arb_changes() -> impl Strategy<Value = BTreeMap<RepoPath, FileChange>> {
    prop::collection::btree_map(arb_path(), arb_change(), 0..6)
}

/// Counts metadata reads and writes so tests can bound the work an ancestry
/// query does and the number of tree and history nodes rewritten.
#[derive(Debug)]
pub struct CountingMeta<'a> {
    pub inner: &'a MemoryStore,
    pub derived_reads: AtomicUsize,
    pub changeset_reads: AtomicUsize,
    pub queries: AtomicUsize,
    pub tree_puts: AtomicUsize,
    pub tree_gets: AtomicUsize,
    pub page_puts: AtomicUsize,
    pub page_gets: AtomicUsize,
}

impl<'a> CountingMeta<'a> {
    pub fn new(inner: &'a MemoryStore) -> Self {
        Self {
            inner,
            derived_reads: AtomicUsize::new(0),
            changeset_reads: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
            tree_puts: AtomicUsize::new(0),
            tree_gets: AtomicUsize::new(0),
            page_puts: AtomicUsize::new(0),
            page_gets: AtomicUsize::new(0),
        }
    }
    /// Trees written, however many calls wrote them.
    pub fn tree_puts(&self) -> usize {
        self.tree_puts.load(Ordering::Relaxed)
    }
    /// Trees read, however many calls read them.
    pub fn tree_gets(&self) -> usize {
        self.tree_gets.load(Ordering::Relaxed)
    }
    /// Path-history pages written.
    pub fn page_puts(&self) -> usize {
        self.page_puts.load(Ordering::Relaxed)
    }
    /// Path-history pages read.
    pub fn page_gets(&self) -> usize {
        self.page_gets.load(Ordering::Relaxed)
    }
    pub fn derived_reads(&self) -> usize {
        self.derived_reads.load(Ordering::Relaxed)
    }
    pub fn changeset_reads(&self) -> usize {
        self.changeset_reads.load(Ordering::Relaxed)
    }
    /// Calls of any kind, each one Postgres round trip on a real store.
    pub fn queries(&self) -> usize {
        self.queries.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl MetadataStore for CountingMeta<'_> {
    async fn create_repo(&self, name: &str) -> scion_store::Result<()> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.create_repo(name).await
    }
    async fn put_changesets(&self, batch: &[Changeset]) -> scion_store::Result<()> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.put_changesets(batch).await
    }
    async fn put_trees(&self, repo: &str, trees: &[Tree]) -> scion_store::Result<()> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.tree_puts.fetch_add(trees.len(), Ordering::Relaxed);
        self.inner.put_trees(repo, trees).await
    }
    async fn get_trees(
        &self,
        repo: &str,
        ids: &[TreeId],
    ) -> scion_store::Result<Vec<Option<Tree>>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.tree_gets.fetch_add(ids.len(), Ordering::Relaxed);
        self.inner.get_trees(repo, ids).await
    }
    async fn put_pages(&self, repo: &str, pages: &[Bytes]) -> scion_store::Result<()> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.page_puts.fetch_add(pages.len(), Ordering::Relaxed);
        self.inner.put_pages(repo, pages).await
    }
    async fn get_pages(
        &self,
        repo: &str,
        ids: &[BlobId],
    ) -> scion_store::Result<Vec<Option<Bytes>>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.page_gets.fetch_add(ids.len(), Ordering::Relaxed);
        self.inner.get_pages(repo, ids).await
    }
    async fn get_changeset(
        &self,
        repo: &str,
        id: ChangesetId,
    ) -> scion_store::Result<Option<Changeset>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.changeset_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get_changeset(repo, id).await
    }
    async fn get_changesets(
        &self,
        repo: &str,
        ids: &[ChangesetId],
    ) -> scion_store::Result<Vec<Option<Changeset>>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.changeset_reads.fetch_add(ids.len(), Ordering::Relaxed);
        self.inner.get_changesets(repo, ids).await
    }
    async fn generation(&self, repo: &str, id: ChangesetId) -> scion_store::Result<Option<u64>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.generation(repo, id).await
    }
    async fn create_ref(
        &self,
        repo: &str,
        name: &str,
        target: ChangesetId,
    ) -> scion_store::Result<RefState> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.create_ref(repo, name, target).await
    }
    async fn get_ref(&self, repo: &str, name: &str) -> scion_store::Result<Option<RefState>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.get_ref(repo, name).await
    }
    async fn move_ref(
        &self,
        repo: &str,
        name: &str,
        expected_version: u64,
        new_target: ChangesetId,
    ) -> scion_store::Result<RefState> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner
            .move_ref(repo, name, expected_version, new_target)
            .await
    }
    async fn list_refs(&self, repo: &str) -> scion_store::Result<Vec<(String, RefState)>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.list_refs(repo).await
    }
    async fn put_derived_many(
        &self,
        repo: &str,
        pointers: Vec<DerivedPointer>,
    ) -> scion_store::Result<()> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.put_derived_many(repo, pointers).await
    }
    async fn get_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
    ) -> scion_store::Result<Option<Vec<u8>>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.derived_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get_derived(repo, ty, version, cs).await
    }
    async fn put_ingest_checkpoints(
        &self,
        repo: &str,
        done: &[(Vec<u8>, ChangesetId)],
    ) -> scion_store::Result<()> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.put_ingest_checkpoints(repo, done).await
    }
    async fn ingest_checkpoints(
        &self,
        repo: &str,
    ) -> scion_store::Result<Vec<(Vec<u8>, ChangesetId)>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.ingest_checkpoints(repo).await
    }

    async fn submit_change_request(
        &self,
        repo: &str,
        head: ChangesetId,
        base: ChangesetId,
        submitter: &str,
    ) -> scion_store::Result<ChangeRequest> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner
            .submit_change_request(repo, head, base, submitter)
            .await
    }

    async fn get_change_request(
        &self,
        repo: &str,
        id: u64,
    ) -> scion_store::Result<Option<ChangeRequest>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.get_change_request(repo, id).await
    }

    async fn list_change_requests(
        &self,
        repo: &str,
        status: Option<ChangeRequestStatus>,
        limit: u32,
    ) -> scion_store::Result<Vec<ChangeRequest>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.list_change_requests(repo, status, limit).await
    }

    async fn find_landings(
        &self,
        repo: &str,
        changesets: &[ChangesetId],
    ) -> scion_store::Result<Vec<(ChangesetId, ChangeRequest)>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.find_landings(repo, changesets).await
    }

    async fn claim_next_change_request(
        &self,
        repo: &str,
    ) -> scion_store::Result<Option<ChangeRequest>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.claim_next_change_request(repo).await
    }

    async fn landing_change_request(
        &self,
        repo: &str,
    ) -> scion_store::Result<Option<ChangeRequest>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.landing_change_request(repo).await
    }

    async fn finish_change_request(
        &self,
        repo: &str,
        id: u64,
        outcome: LandOutcome,
    ) -> scion_store::Result<ChangeRequest> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.finish_change_request(repo, id, outcome).await
    }

    async fn reject_change_request(
        &self,
        repo: &str,
        id: u64,
    ) -> scion_store::Result<ChangeRequest> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.reject_change_request(repo, id).await
    }

    async fn repos_with_open_change_requests(&self) -> scion_store::Result<Vec<String>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.repos_with_open_change_requests().await
    }

    async fn put_grant(&self, repo: &str, grant: &Grant) -> scion_store::Result<()> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.put_grant(repo, grant).await
    }

    async fn delete_grant(
        &self,
        repo: &str,
        principal: &str,
        path_prefix: &str,
    ) -> scion_store::Result<bool> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.delete_grant(repo, principal, path_prefix).await
    }

    async fn grants_for(&self, repo: &str, principal: &str) -> scion_store::Result<Vec<Grant>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.grants_for(repo, principal).await
    }

    async fn grants(&self, repo: &str) -> scion_store::Result<Vec<Grant>> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.grants(repo).await
    }
    async fn repos_of(&self, principal: &str) -> scion_store::Result<Vec<String>> {
        self.inner.repos_of(principal).await
    }
}
