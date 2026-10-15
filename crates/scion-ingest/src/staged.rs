//! Write-behind staging of the stores for one import.
//!
//! Creating a changeset reads what the previous one wrote (its trees, path
//! history and derived pointers) and writes a little more. Against Postgres
//! and S3 every one of those reads and writes is a round trip, and done in
//! line they dominate an import. [`Staged`] answers the reads from memory and
//! hands the writes to [`write_behind`], which runs object writes
//! concurrently and metadata writes in order. A metadata write starts only
//! once every write queued before it is stored, so the stores never hold a
//! changeset, derived pointer or checkpoint whose objects are missing, and an
//! interrupted import still resumes from its last checkpoint.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use scion_store::{MetadataStore, ObjectStore, RefState, Result, StoreError};
use scion_types::{BlobId, Changeset, ChangesetId, Tree, TreeId};
use tokio::sync::mpsc;

use crate::{IngestError, ObjectId};

/// Most object writes in flight at once.
const UPLOAD_CONCURRENCY: usize = 64;

/// Writes queued ahead of the stores. Bounds the importer's memory to this
/// many writes on top of the largest commit.
pub(crate) const QUEUE_DEPTH: usize = 1024;

/// Written trees and path-history nodes kept for the next changesets, which
/// rewrite the same directories. Past this, the least recently used go.
const CACHE_BYTES: usize = 64 << 20;

/// One queued write, with the git commit it belongs to for error messages.
pub(crate) struct Write {
    commit: ObjectId,
    op: Op,
}

enum Op {
    Blob(Bytes),
    Tree(Tree),
    Changeset(Box<Changeset>),
    Derived {
        repo: String,
        ty: String,
        version: u32,
        cs: ChangesetId,
        pointer: Vec<u8>,
    },
    Checkpoint {
        repo: String,
        sha: Vec<u8>,
        cs: ChangesetId,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Blob(BlobId),
    Tree(TreeId),
}

#[derive(Clone)]
enum Object {
    Blob(Bytes),
    Tree(Tree),
}

struct Entry {
    object: Object,
    bytes: usize,
    used: u64,
    /// Queued writes of this object not yet stored. While non-zero the
    /// backend may not have it, so it is never evicted.
    pending: u32,
}

#[derive(Default)]
pub(crate) struct Cache {
    commit: Option<ObjectId>,
    objects: HashMap<Key, Entry>,
    bytes: usize,
    clock: u64,
    /// File contents of the current commit: queued or stored.
    files: HashSet<BlobId>,
    /// Changesets queued and not yet stored.
    changesets: HashMap<ChangesetId, Changeset>,
    /// Every changeset this import created. Their derived values can only
    /// come from this import, so `derived` alone answers for them.
    created: HashSet<ChangesetId>,
    /// Derived pointers written or read by this import; a few ids each, kept
    /// whole because skip lists reach far back.
    derived: HashMap<(String, u32, ChangesetId), Vec<u8>>,
}

impl Cache {
    fn get(&mut self, key: Key) -> Option<Object> {
        self.clock += 1;
        let entry = self.objects.get_mut(&key)?;
        entry.used = self.clock;
        Some(entry.object.clone())
    }

    /// Cache `object`; `pending` when a write of it is being queued.
    fn insert(&mut self, key: Key, object: Object, pending: bool) {
        self.clock += 1;
        let bytes = match &object {
            Object::Blob(data) => data.len(),
            Object::Tree(tree) => 64 * tree.entries.len(),
        };
        let entry = self.objects.entry(key).or_insert_with(|| {
            self.bytes += bytes;
            Entry {
                object,
                bytes,
                used: 0,
                pending: 0,
            }
        });
        entry.used = self.clock;
        entry.pending += u32::from(pending);
        if self.bytes > CACHE_BYTES {
            self.evict();
        }
    }

    fn stored(&mut self, key: Key) {
        if let Some(entry) = self.objects.get_mut(&key) {
            entry.pending -= 1;
        }
    }

    /// Drop the least recently used half of the stored objects.
    fn evict(&mut self) {
        let mut used: Vec<u64> = self
            .objects
            .values()
            .filter(|e| e.pending == 0)
            .map(|e| e.used)
            .collect();
        if used.is_empty() {
            return;
        }
        let mid = used.len() / 2;
        let cutoff = *used.select_nth_unstable(mid).1;
        let mut freed = 0;
        self.objects.retain(|_, e| {
            let keep = e.pending > 0 || e.used > cutoff;
            if !keep {
                freed += e.bytes;
            }
            keep
        });
        self.bytes -= freed;
    }
}

/// The stores as one import sees them: reads from memory where possible,
/// writes queued for [`write_behind`].
pub(crate) struct Staged<'a> {
    meta: &'a dyn MetadataStore,
    objects: &'a dyn ObjectStore,
    cache: &'a Mutex<Cache>,
    queue: mpsc::Sender<Write>,
}

/// Shared between [`Staged`] and [`write_behind`].
pub(crate) type Shared = Mutex<Cache>;

impl<'a> Staged<'a> {
    pub(crate) fn new(
        meta: &'a dyn MetadataStore,
        objects: &'a dyn ObjectStore,
        cache: &'a Shared,
        queue: mpsc::Sender<Write>,
    ) -> Self {
        Self {
            meta,
            objects,
            cache,
            queue,
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().expect("poisoned")
    }

    /// Start staging git commit `commit`.
    pub(crate) fn begin(&self, commit: ObjectId) {
        let mut cache = self.cache();
        cache.commit = Some(commit);
        cache.files.clear();
    }

    /// Queue the contents of a file of the current commit. Unlike
    /// [`ObjectStore::put_blob`] it is not kept in memory once stored: file
    /// contents are not read again by the import.
    pub(crate) async fn put_file(&self, id: BlobId, data: Bytes) -> Result<()> {
        self.cache().files.insert(id);
        self.send(Op::Blob(data)).await
    }

    async fn send(&self, op: Op) -> Result<()> {
        let commit = self.cache().commit.expect("begin() before any write");
        self.queue
            .send(Write { commit, op })
            .await
            .map_err(|_| StoreError::Backend("the import's writer stopped".into()))
    }
}

type UploadFuture<'a> =
    std::pin::Pin<Box<dyn Future<Output = (ObjectId, Result<Key>)> + Send + 'a>>;

/// Store one object write; returns the key it is cached under.
fn upload(objects: &dyn ObjectStore, commit: ObjectId, op: Op) -> UploadFuture<'_> {
    Box::pin(async move {
        let done = match op {
            Op::Blob(data) => objects.put_blob(data).await.map(Key::Blob),
            Op::Tree(tree) => objects.put_tree(&tree).await.map(Key::Tree),
            _ => unreachable!("only objects are uploaded"),
        };
        (commit, done)
    })
}

/// Store every queued write, object writes concurrently, metadata writes in
/// queue order once everything queued before them is stored. Returns when the
/// queue is closed and drained, or at the first failed write.
pub(crate) async fn write_behind(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    cache: &Shared,
    mut queue: mpsc::Receiver<Write>,
) -> std::result::Result<(), IngestError> {
    let err = |commit: ObjectId| move |e: StoreError| IngestError::Store(commit, e.to_string());
    let stored = |key: Key| cache.lock().expect("poisoned").stored(key);
    let mut uploads = FuturesUnordered::<UploadFuture<'_>>::new();
    loop {
        tokio::select! {
            Some((commit, done)) = uploads.next() => stored(done.map_err(err(commit))?),
            write = queue.recv(), if uploads.len() < UPLOAD_CONCURRENCY => {
                let Some(Write { commit, op }) = write else { break };
                match op {
                    op @ (Op::Blob(_) | Op::Tree(_)) => uploads.push(upload(objects, commit, op)),
                    op => {
                        while let Some((commit, done)) = uploads.next().await {
                            stored(done.map_err(err(commit))?);
                        }
                        match op {
                            Op::Changeset(cs) => {
                                meta.put_changeset(&cs).await.map_err(err(commit))?;
                                cache.lock().expect("poisoned").changesets.remove(&cs.id());
                            }
                            Op::Derived { repo, ty, version, cs, pointer } => meta
                                .put_derived(&repo, &ty, version, cs, pointer)
                                .await
                                .map_err(err(commit))?,
                            Op::Checkpoint { repo, sha, cs } => meta
                                .put_ingest_checkpoint(&repo, &sha, cs)
                                .await
                                .map_err(err(commit))?,
                            Op::Blob(_) | Op::Tree(_) => unreachable!("handled above"),
                        }
                    }
                }
            }
            else => break,
        }
    }
    while let Some((commit, done)) = uploads.next().await {
        stored(done.map_err(err(commit))?);
    }
    Ok(())
}

#[async_trait]
impl MetadataStore for Staged<'_> {
    async fn put_changeset(&self, cs: &Changeset) -> Result<ChangesetId> {
        let id = cs.id();
        {
            let mut cache = self.cache();
            cache.changesets.insert(id, cs.clone());
            cache.created.insert(id);
        }
        self.send(Op::Changeset(Box::new(cs.clone()))).await?;
        Ok(id)
    }

    async fn get_changeset(&self, repo: &str, id: ChangesetId) -> Result<Option<Changeset>> {
        if let Some(cs) = self.cache().changesets.get(&id) {
            return Ok(Some(cs.clone()));
        }
        self.meta.get_changeset(repo, id).await
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

    async fn put_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
        pointer: Vec<u8>,
    ) -> Result<()> {
        self.cache()
            .derived
            .insert((ty.to_owned(), version, cs), pointer.clone());
        self.send(Op::Derived {
            repo: repo.to_owned(),
            ty: ty.to_owned(),
            version,
            cs,
            pointer,
        })
        .await
    }

    async fn get_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
    ) -> Result<Option<Vec<u8>>> {
        let key = (ty.to_owned(), version, cs);
        {
            let cache = self.cache();
            if let Some(pointer) = cache.derived.get(&key) {
                return Ok(Some(pointer.clone()));
            }
            if cache.created.contains(&cs) {
                return Ok(None);
            }
        }
        let pointer = self.meta.get_derived(repo, ty, version, cs).await?;
        if let Some(p) = &pointer {
            self.cache().derived.insert(key, p.clone());
        }
        Ok(pointer)
    }

    async fn put_ingest_checkpoint(
        &self,
        repo: &str,
        git_sha: &[u8],
        cs: ChangesetId,
    ) -> Result<()> {
        self.send(Op::Checkpoint {
            repo: repo.to_owned(),
            sha: git_sha.to_vec(),
            cs,
        })
        .await
    }

    async fn ingest_checkpoints(&self, repo: &str) -> Result<Vec<(Vec<u8>, ChangesetId)>> {
        self.meta.ingest_checkpoints(repo).await
    }
}

#[async_trait]
impl ObjectStore for Staged<'_> {
    async fn put_blob(&self, bytes: Bytes) -> Result<BlobId> {
        let id = BlobId::hash(&bytes);
        self.cache()
            .insert(Key::Blob(id), Object::Blob(bytes.clone()), true);
        self.send(Op::Blob(bytes)).await?;
        Ok(id)
    }

    async fn get_blob(&self, id: BlobId) -> Result<Option<Bytes>> {
        if let Some(Object::Blob(data)) = self.cache().get(Key::Blob(id)) {
            return Ok(Some(data));
        }
        let data = self.objects.get_blob(id).await?;
        if let Some(d) = &data {
            self.cache()
                .insert(Key::Blob(id), Object::Blob(d.clone()), false);
        }
        Ok(data)
    }

    async fn has_blobs(&self, ids: &[BlobId]) -> Result<Vec<bool>> {
        let known: Vec<bool> = {
            let cache = self.cache();
            ids.iter()
                .map(|id| cache.files.contains(id) || cache.objects.contains_key(&Key::Blob(*id)))
                .collect()
        };
        let unknown: Vec<BlobId> = ids
            .iter()
            .zip(&known)
            .filter(|(_, k)| !**k)
            .map(|(id, _)| *id)
            .collect();
        if unknown.is_empty() {
            return Ok(known);
        }
        let mut present = self.objects.has_blobs(&unknown).await?.into_iter();
        Ok(known
            .into_iter()
            .map(|k| k || present.next().unwrap_or(false))
            .collect())
    }

    async fn put_tree(&self, tree: &Tree) -> Result<TreeId> {
        let id = tree.id();
        self.cache()
            .insert(Key::Tree(id), Object::Tree(tree.clone()), true);
        self.send(Op::Tree(tree.clone())).await?;
        Ok(id)
    }

    async fn get_tree(&self, id: TreeId) -> Result<Option<Tree>> {
        if let Some(Object::Tree(tree)) = self.cache().get(Key::Tree(id)) {
            return Ok(Some(tree));
        }
        let tree = self.objects.get_tree(id).await?;
        if let Some(t) = &tree {
            self.cache()
                .insert(Key::Tree(id), Object::Tree(t.clone()), false);
        }
        Ok(tree)
    }
}
