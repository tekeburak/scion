//! The stores as an import sees them: a bounded cache in front, writes
//! queued behind.
//!
//! Creating a changeset derives its root tree, skip list and path history
//! from its parent's, which the previous commit of the same import has just
//! written. [`Stores`] answers those reads from memory and returns from every
//! write at once; [`Stores::write`] performs the writes concurrently, in an
//! order that keeps the stores as consistent as synchronous writes would:
//!
//! * a changeset, derived pointer or checkpoint is written only after every
//!   object queued before it is stored, so no metadata ever names a missing
//!   object;
//! * a changeset or checkpoint is written only after every changeset queued
//!   before it, so parents are stored first and a checkpoint never names a
//!   missing changeset;
//! * if a write fails, nothing that depends on it is written, everything
//!   else already queued is, and the import fails with that error. Resuming
//!   from the checkpoints then redoes exactly the rest.
//!
//! So a commit costs its own writes, mostly in parallel with the next
//! commits, however long the history. Only immutable values are cached:
//! objects, changesets and derived pointers never change once written
//! (invariant 9), so a cached value is never stale.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use scion_store::{MetadataStore, ObjectStore, RefState, Result, StoreError};
use scion_types::{BlobId, Changeset, ChangesetId, Tree, TreeId};
use tokio::sync::{mpsc, oneshot};

/// Bytes the cache may hold, roughly; memory stays bounded however long the
/// history is.
const BUDGET: usize = 256 << 20;

/// Largest blob whose contents are cached. Path history nodes are small;
/// file contents are not read back during an import.
const MAX_CACHED_BLOB: usize = 1 << 20;

/// Fixed per-entry overhead counted against [`BUDGET`].
const ENTRY_COST: usize = 64;

/// Writes queued and not yet picked up by the writer.
const QUEUE: usize = 256;

/// Writes the writer holds at once, running or waiting for earlier ones.
const IN_FLIGHT: usize = 256;

/// Writes running at once.
const RUNNING: usize = 64;

/// A two-generation cache: entries go into `young`; when it fills half the
/// budget it becomes `old` and the previous `old` is dropped. A hit in `old`
/// moves the entry back to `young`, so whatever the import keeps reading
/// (recent trees, skip-list ancestors) survives.
struct Generations<K, V> {
    young: HashMap<K, (V, usize)>,
    old: HashMap<K, (V, usize)>,
    young_bytes: usize,
}

impl<K: Hash + Eq, V: Clone> Generations<K, V> {
    fn new() -> Self {
        Self {
            young: HashMap::new(),
            old: HashMap::new(),
            young_bytes: 0,
        }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        if let Some((v, _)) = self.young.get(key) {
            return Some(v.clone());
        }
        let (key, (v, cost)) = self.old.remove_entry(key)?;
        self.insert(key, v.clone(), cost);
        Some(v)
    }

    fn insert(&mut self, key: K, value: V, cost: usize) {
        if self.young.insert(key, (value, cost)).is_none() {
            self.young_bytes += cost;
        }
        if self.young_bytes > BUDGET / 2 {
            self.old = std::mem::take(&mut self.young);
            self.young_bytes = 0;
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Blob(BlobId),
    Tree(TreeId),
    Changeset(String, ChangesetId),
    /// A changeset this import created: it has no derived data unless
    /// this import derived it.
    Created(String, ChangesetId),
    Derived(String, String, u32, ChangesetId),
}

#[derive(Clone)]
enum Value {
    /// A blob known to be stored, contents not kept; or a [`Key::Created`].
    Present,
    Blob(Bytes),
    Tree(Tree),
    Changeset(Box<Changeset>),
    Derived(Vec<u8>),
}

impl Value {
    fn cost(&self) -> usize {
        ENTRY_COST
            + match self {
                Value::Present => 0,
                Value::Blob(b) => b.len(),
                Value::Tree(t) => t.entries.len() * ENTRY_COST,
                Value::Changeset(cs) => cs.file_changes.len() * ENTRY_COST + cs.message.len(),
                Value::Derived(p) => p.len(),
            }
    }
}

/// A queued write.
enum Write {
    Blob(Bytes),
    Tree(Tree),
    Changeset(Changeset),
    Derived {
        repo: String,
        ty: String,
        version: u32,
        cs: ChangesetId,
        pointer: Vec<u8>,
    },
    Checkpoint {
        repo: String,
        git_sha: Vec<u8>,
        cs: ChangesetId,
    },
    /// Answered once everything queued before it is stored.
    Flush(oneshot::Sender<()>),
}

/// What a write must wait for; see the module docs.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Class {
    Object,
    Changeset,
    Derived,
    Checkpoint,
    Flush,
}

impl Class {
    fn waits_for(self) -> &'static [Class] {
        match self {
            Class::Object => &[],
            Class::Derived => &[Class::Object],
            Class::Changeset | Class::Checkpoint => &[Class::Object, Class::Changeset],
            Class::Flush => &[
                Class::Object,
                Class::Changeset,
                Class::Derived,
                Class::Checkpoint,
            ],
        }
    }
}

impl Write {
    fn class(&self) -> Class {
        match self {
            Write::Blob(_) | Write::Tree(_) => Class::Object,
            Write::Changeset(_) => Class::Changeset,
            Write::Derived { .. } => Class::Derived,
            Write::Checkpoint { .. } => Class::Checkpoint,
            Write::Flush(_) => Class::Flush,
        }
    }
}

fn stopped() -> StoreError {
    StoreError::Backend("the import's writer stopped".into())
}

/// The stores an import writes to, with a cache in front and the writes
/// queued for [`Stores::write`].
pub(crate) struct Stores<'a> {
    meta: &'a dyn MetadataStore,
    objects: &'a dyn ObjectStore,
    cache: Mutex<Generations<Key, Value>>,
    /// Queued and not yet stored; never evicted, so reads find them.
    pending: Mutex<HashMap<Key, Value>>,
    queue: Mutex<Option<mpsc::Sender<Write>>>,
}

/// The receiving end of a [`Stores`]' queue.
pub(crate) struct Queue(mpsc::Receiver<Write>);

impl<'a> Stores<'a> {
    pub(crate) fn new(meta: &'a dyn MetadataStore, objects: &'a dyn ObjectStore) -> (Self, Queue) {
        let (tx, rx) = mpsc::channel(QUEUE);
        let stores = Self {
            meta,
            objects,
            cache: Mutex::new(Generations::new()),
            pending: Mutex::new(HashMap::new()),
            queue: Mutex::new(Some(tx)),
        };
        (stores, Queue(rx))
    }

    fn get(&self, key: &Key) -> Option<Value> {
        if let Some(v) = self.pending.lock().expect("pending lock").get(key) {
            return Some(v.clone());
        }
        self.cache.lock().expect("cache lock").get(key)
    }

    fn insert(&self, key: Key, value: Value) {
        let cost = value.cost();
        self.cache
            .lock()
            .expect("cache lock")
            .insert(key, value, cost);
    }

    /// Cache `value` and queue `write`, which stores it.
    async fn queue(&self, key: Key, value: Value, write: Write) -> Result<()> {
        self.pending
            .lock()
            .expect("pending lock")
            .insert(key.clone(), value.clone());
        self.insert(key, value);
        self.send(write).await
    }

    async fn send(&self, write: Write) -> Result<()> {
        let tx = self.queue.lock().expect("queue lock").clone();
        tx.ok_or_else(stopped)?
            .send(write)
            .await
            .map_err(|_| stopped())
    }

    /// Wait until everything queued so far is stored.
    pub(crate) async fn flush(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Write::Flush(tx)).await?;
        rx.await.map_err(|_| stopped())
    }

    /// No more writes: [`Stores::write`] returns once the queue is drained.
    pub(crate) fn close(&self) {
        self.queue.lock().expect("queue lock").take();
    }

    /// Perform the queued writes until [`Stores::close`], in the order the
    /// module docs describe. Returns the first error, after finishing every
    /// queued write that does not depend on the failed one.
    pub(crate) async fn write(&self, Queue(mut rx): Queue) -> Result<()> {
        let mut seq = 0u64;
        // Queued and not yet stored, by class; a failed write stays here,
        // so whatever waits for it never starts.
        let mut unfinished: HashMap<Class, BTreeSet<u64>> = HashMap::new();
        let mut waiting: VecDeque<(u64, Write)> = VecDeque::new();
        let mut running = FuturesUnordered::new();
        let mut open = true;
        let mut failed: Option<StoreError> = None;
        loop {
            // Start every waiting write whose predecessors are stored.
            let mut i = 0;
            while i < waiting.len() && running.len() < RUNNING {
                let (s, class) = (waiting[i].0, waiting[i].1.class());
                let ready = class.waits_for().iter().all(|c| {
                    unfinished
                        .get(c)
                        .is_none_or(|set| set.range(..s).next().is_none())
                });
                if ready {
                    let (s, w) = waiting.remove(i).expect("index in range");
                    running.push(self.perform(s, w));
                } else {
                    i += 1;
                }
            }
            let accepting = open && failed.is_none();
            if !accepting && running.is_empty() {
                break;
            }
            tokio::select! {
                w = rx.recv(), if accepting && running.len() + waiting.len() < IN_FLIGHT => {
                    let Some(w) = w else {
                        open = false;
                        continue;
                    };
                    seq += 1;
                    unfinished.entry(w.class()).or_default().insert(seq);
                    waiting.push_back((seq, w));
                }
                Some((s, class, key, result)) = running.next(), if !running.is_empty() => {
                    match result {
                        Ok(()) => {
                            if let Some(set) = unfinished.get_mut(&class) {
                                set.remove(&s);
                            }
                            if let Some(key) = key {
                                self.pending.lock().expect("pending lock").remove(&key);
                            }
                        }
                        Err(e) => {
                            failed.get_or_insert(e);
                        }
                    }
                }
            }
        }
        failed.map_or(Ok(()), Err)
    }

    /// Store one write; reports its sequence number, class and cache key.
    fn perform(
        &self,
        seq: u64,
        write: Write,
    ) -> BoxFuture<'_, (u64, Class, Option<Key>, Result<()>)> {
        let class = write.class();
        Box::pin(async move {
            let (key, result) = match write {
                Write::Blob(bytes) => {
                    let id = BlobId::hash(&bytes);
                    (
                        Some(Key::Blob(id)),
                        self.objects.put_blob(bytes).await.map(drop),
                    )
                }
                Write::Tree(tree) => (
                    Some(Key::Tree(tree.id())),
                    self.objects.put_tree(&tree).await.map(drop),
                ),
                Write::Changeset(cs) => (
                    Some(Key::Changeset(cs.repo.clone(), cs.id())),
                    self.meta.put_changeset(&cs).await.map(drop),
                ),
                Write::Derived {
                    repo,
                    ty,
                    version,
                    cs,
                    pointer,
                } => {
                    let result = self
                        .meta
                        .put_derived(&repo, &ty, version, cs, pointer)
                        .await;
                    (Some(Key::Derived(repo, ty, version, cs)), result)
                }
                Write::Checkpoint { repo, git_sha, cs } => (
                    None,
                    self.meta.put_ingest_checkpoint(&repo, &git_sha, cs).await,
                ),
                Write::Flush(done) => {
                    // The import may have stopped waiting; that is fine.
                    let _ = done.send(());
                    (None, Ok(()))
                }
            };
            (seq, class, key, result)
        })
    }
}

#[async_trait]
impl ObjectStore for Stores<'_> {
    async fn put_blob(&self, bytes: Bytes) -> Result<BlobId> {
        let id = BlobId::hash(&bytes);
        if self.get(&Key::Blob(id)).is_none() {
            let value = if bytes.len() <= MAX_CACHED_BLOB {
                Value::Blob(bytes.clone())
            } else {
                Value::Present
            };
            self.queue(Key::Blob(id), value, Write::Blob(bytes)).await?;
        }
        Ok(id)
    }

    async fn get_blob(&self, id: BlobId) -> Result<Option<Bytes>> {
        if let Some(Value::Blob(b)) = self.get(&Key::Blob(id)) {
            return Ok(Some(b));
        }
        let got = self.objects.get_blob(id).await?;
        if let Some(b) = got.as_ref().filter(|b| b.len() <= MAX_CACHED_BLOB) {
            self.insert(Key::Blob(id), Value::Blob(b.clone()));
        }
        Ok(got)
    }

    /// Queued blobs count as present: whatever names them is written after
    /// them.
    async fn has_blobs(&self, ids: &[BlobId]) -> Result<Vec<bool>> {
        let mut out: Vec<bool> = ids
            .iter()
            .map(|id| self.get(&Key::Blob(*id)).is_some())
            .collect();
        let unknown: Vec<usize> = (0..ids.len()).filter(|i| !out[*i]).collect();
        if unknown.is_empty() {
            return Ok(out);
        }
        let asked: Vec<BlobId> = unknown.iter().map(|i| ids[*i]).collect();
        for (i, present) in unknown
            .into_iter()
            .zip(self.objects.has_blobs(&asked).await?)
        {
            if present {
                self.insert(Key::Blob(ids[i]), Value::Present);
            }
            out[i] = present;
        }
        Ok(out)
    }

    async fn put_tree(&self, tree: &Tree) -> Result<TreeId> {
        let id = tree.id();
        if self.get(&Key::Tree(id)).is_none() {
            self.queue(
                Key::Tree(id),
                Value::Tree(tree.clone()),
                Write::Tree(tree.clone()),
            )
            .await?;
        }
        Ok(id)
    }

    async fn get_tree(&self, id: TreeId) -> Result<Option<Tree>> {
        if let Some(Value::Tree(t)) = self.get(&Key::Tree(id)) {
            return Ok(Some(t));
        }
        let got = self.objects.get_tree(id).await?;
        if let Some(t) = &got {
            self.insert(Key::Tree(id), Value::Tree(t.clone()));
        }
        Ok(got)
    }
}

#[async_trait]
impl MetadataStore for Stores<'_> {
    /// Parents are not checked here: the import names only parents it has
    /// created or found, and the store checks them when the write runs.
    async fn put_changeset(&self, cs: &Changeset) -> Result<ChangesetId> {
        let id = cs.id();
        let key = Key::Changeset(cs.repo.clone(), id);
        if self.get(&key).is_none() {
            self.insert(Key::Created(cs.repo.clone(), id), Value::Present);
            self.queue(
                key,
                Value::Changeset(Box::new(cs.clone())),
                Write::Changeset(cs.clone()),
            )
            .await?;
        }
        Ok(id)
    }

    async fn get_changeset(&self, repo: &str, id: ChangesetId) -> Result<Option<Changeset>> {
        let key = Key::Changeset(repo.to_owned(), id);
        if let Some(Value::Changeset(cs)) = self.get(&key) {
            return Ok(Some(*cs));
        }
        let got = self.meta.get_changeset(repo, id).await?;
        if let Some(cs) = &got {
            self.insert(key, Value::Changeset(Box::new(cs.clone())));
        }
        Ok(got)
    }

    async fn generation(&self, repo: &str, id: ChangesetId) -> Result<Option<u64>> {
        self.flush().await?;
        self.meta.generation(repo, id).await
    }

    async fn create_ref(&self, repo: &str, name: &str, target: ChangesetId) -> Result<RefState> {
        self.flush().await?;
        self.meta.create_ref(repo, name, target).await
    }

    async fn get_ref(&self, repo: &str, name: &str) -> Result<Option<RefState>> {
        self.flush().await?;
        self.meta.get_ref(repo, name).await
    }

    async fn move_ref(
        &self,
        repo: &str,
        name: &str,
        expected_version: u64,
        new_target: ChangesetId,
    ) -> Result<RefState> {
        self.flush().await?;
        self.meta
            .move_ref(repo, name, expected_version, new_target)
            .await
    }

    async fn list_refs(&self, repo: &str) -> Result<Vec<(String, RefState)>> {
        self.flush().await?;
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
        let key = Key::Derived(repo.to_owned(), ty.to_owned(), version, cs);
        if self.get(&key).is_none() {
            let write = Write::Derived {
                repo: repo.to_owned(),
                ty: ty.to_owned(),
                version,
                cs,
                pointer: pointer.clone(),
            };
            self.queue(key, Value::Derived(pointer), write).await?;
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
        let key = Key::Derived(repo.to_owned(), ty.to_owned(), version, cs);
        if let Some(Value::Derived(p)) = self.get(&key) {
            return Ok(Some(p));
        }
        // Derived values are a function of the changeset: if an earlier run
        // stored one for a changeset this import created, deriving it again
        // gives the same value.
        if self.get(&Key::Created(repo.to_owned(), cs)).is_some() {
            return Ok(None);
        }
        let got = self.meta.get_derived(repo, ty, version, cs).await?;
        if let Some(p) = &got {
            self.insert(key, Value::Derived(p.clone()));
        }
        Ok(got)
    }

    async fn put_ingest_checkpoint(
        &self,
        repo: &str,
        git_sha: &[u8],
        cs: ChangesetId,
    ) -> Result<()> {
        self.send(Write::Checkpoint {
            repo: repo.to_owned(),
            git_sha: git_sha.to_vec(),
            cs,
        })
        .await
    }

    async fn ingest_checkpoints(&self, repo: &str) -> Result<Vec<(Vec<u8>, ChangesetId)>> {
        self.flush().await?;
        self.meta.ingest_checkpoints(repo).await
    }
}
