//! Shared test helpers: naive materialisation, changeset builders, a counting
//! object store, and proptest generators.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use proptest::prelude::*;
use scion_store::{MemoryStore, MetadataStore, ObjectStore, RefState};
use scion_types::{
    BlobId, Changeset, ChangesetId, FileChange, FileType, Identity, RepoPath, Timestamp, Tree,
    TreeEntry, TreeId,
};

pub const REPO: &str = "t";

/// `path -> file`: the naive, fully materialised view of a tree.
pub type Flat = BTreeMap<RepoPath, (BlobId, FileType, u64)>;

/// Flatten a tree into `path -> file`.
pub async fn materialize(objects: &dyn ObjectStore, root: TreeId) -> Flat {
    let mut out = Flat::new();
    let mut stack = vec![(None::<RepoPath>, root)];
    while let Some((prefix, id)) = stack.pop() {
        let tree = objects.get_tree(id).await.unwrap().expect("tree exists");
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

/// Counts writes so tests can bound the number of rewritten nodes.
#[derive(Debug)]
pub struct Counting<'a> {
    pub inner: &'a MemoryStore,
    pub tree_puts: AtomicUsize,
    pub blob_puts: AtomicUsize,
    pub tree_gets: AtomicUsize,
}

impl<'a> Counting<'a> {
    pub fn new(inner: &'a MemoryStore) -> Self {
        Self {
            inner,
            tree_puts: AtomicUsize::new(0),
            blob_puts: AtomicUsize::new(0),
            tree_gets: AtomicUsize::new(0),
        }
    }
    pub fn tree_puts(&self) -> usize {
        self.tree_puts.load(Ordering::Relaxed)
    }
    pub fn blob_puts(&self) -> usize {
        self.blob_puts.load(Ordering::Relaxed)
    }
    pub fn tree_gets(&self) -> usize {
        self.tree_gets.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ObjectStore for Counting<'_> {
    async fn put_blob(&self, bytes: Bytes) -> scion_store::Result<BlobId> {
        self.blob_puts.fetch_add(1, Ordering::Relaxed);
        self.inner.put_blob(bytes).await
    }
    async fn get_blob(&self, id: BlobId) -> scion_store::Result<Option<Bytes>> {
        self.inner.get_blob(id).await
    }
    async fn has_blobs(&self, ids: &[BlobId]) -> scion_store::Result<Vec<bool>> {
        self.inner.has_blobs(ids).await
    }
    async fn put_tree(&self, tree: &Tree) -> scion_store::Result<TreeId> {
        self.tree_puts.fetch_add(1, Ordering::Relaxed);
        self.inner.put_tree(tree).await
    }
    async fn get_tree(&self, id: TreeId) -> scion_store::Result<Option<Tree>> {
        self.tree_gets.fetch_add(1, Ordering::Relaxed);
        self.inner.get_tree(id).await
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

/// Counts metadata reads so tests can bound the work an ancestry query does.
#[derive(Debug)]
pub struct CountingMeta<'a> {
    pub inner: &'a MemoryStore,
    pub derived_reads: AtomicUsize,
    pub changeset_reads: AtomicUsize,
}

impl<'a> CountingMeta<'a> {
    pub fn new(inner: &'a MemoryStore) -> Self {
        Self {
            inner,
            derived_reads: AtomicUsize::new(0),
            changeset_reads: AtomicUsize::new(0),
        }
    }
    pub fn derived_reads(&self) -> usize {
        self.derived_reads.load(Ordering::Relaxed)
    }
    pub fn changeset_reads(&self) -> usize {
        self.changeset_reads.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl MetadataStore for CountingMeta<'_> {
    async fn put_changeset(&self, cs: &Changeset) -> scion_store::Result<ChangesetId> {
        self.inner.put_changeset(cs).await
    }
    async fn get_changeset(
        &self,
        repo: &str,
        id: ChangesetId,
    ) -> scion_store::Result<Option<Changeset>> {
        self.changeset_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get_changeset(repo, id).await
    }
    async fn generation(&self, repo: &str, id: ChangesetId) -> scion_store::Result<Option<u64>> {
        self.inner.generation(repo, id).await
    }
    async fn create_ref(
        &self,
        repo: &str,
        name: &str,
        target: ChangesetId,
    ) -> scion_store::Result<RefState> {
        self.inner.create_ref(repo, name, target).await
    }
    async fn get_ref(&self, repo: &str, name: &str) -> scion_store::Result<Option<RefState>> {
        self.inner.get_ref(repo, name).await
    }
    async fn move_ref(
        &self,
        repo: &str,
        name: &str,
        expected_version: u64,
        new_target: ChangesetId,
    ) -> scion_store::Result<RefState> {
        self.inner
            .move_ref(repo, name, expected_version, new_target)
            .await
    }
    async fn list_refs(&self, repo: &str) -> scion_store::Result<Vec<(String, RefState)>> {
        self.inner.list_refs(repo).await
    }
    async fn put_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
        pointer: Vec<u8>,
    ) -> scion_store::Result<()> {
        self.inner.put_derived(repo, ty, version, cs, pointer).await
    }
    async fn get_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
    ) -> scion_store::Result<Option<Vec<u8>>> {
        self.derived_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get_derived(repo, ty, version, cs).await
    }
    async fn put_ingest_checkpoint(
        &self,
        repo: &str,
        git_sha: &[u8],
        cs: ChangesetId,
    ) -> scion_store::Result<()> {
        self.inner.put_ingest_checkpoint(repo, git_sha, cs).await
    }
    async fn ingest_checkpoints(
        &self,
        repo: &str,
    ) -> scion_store::Result<Vec<(Vec<u8>, ChangesetId)>> {
        self.inner.ingest_checkpoints(repo).await
    }
}
