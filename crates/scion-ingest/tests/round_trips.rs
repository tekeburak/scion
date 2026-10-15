//! What one imported commit costs the stores: a constant number of calls,
//! whatever the length of the history. Every call counted here is one
//! round trip to Postgres or the object store in production.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use async_trait::async_trait;
use bytes::Bytes;
use scion_ingest::{GitRepo, import};
use scion_store::{MemoryStore, MetadataStore, ObjectStore, RefState};
use scion_types::{BlobId, Changeset, ChangesetId, Tree, TreeId};

const REPO: &str = "counted";
/// Directory levels above each edited file; every commit rewrites one tree
/// and one path-history node per level, plus the root's.
const DEPTH: usize = 3;

/// A linear history in which commit `i` rewrites one of 50 files, each
/// `DEPTH` directories deep, written with `git fast-import`.
fn generate(dir: &Path, commits: usize) {
    let mut stream = Vec::new();
    for i in 1..=commits {
        let body = format!("version {i}\n");
        writeln!(stream, "commit refs/heads/main").unwrap();
        writeln!(stream, "mark :{i}").unwrap();
        writeln!(
            stream,
            "committer Gen <gen@example.com> {} +0000",
            1_700_000_000 + i
        )
        .unwrap();
        writeln!(stream, "data 2\nc\n").unwrap();
        if i > 1 {
            writeln!(stream, "from :{}", i - 1).unwrap();
        }
        let n = i % 50;
        writeln!(
            stream,
            "M 100644 inline a{}/b{}/c/file{n}.txt",
            n % 2,
            n % 5
        )
        .unwrap();
        writeln!(stream, "data {}\n{body}", body.len()).unwrap();
    }
    let ok = |c: &mut Command| assert!(c.status().unwrap().success());
    ok(Command::new("git")
        .current_dir(dir)
        .args(["init", "-q", "-b", "main"]));
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("git is installed");
    child.stdin.take().unwrap().write_all(&stream).unwrap();
    assert!(child.wait().unwrap().success());
}

/// Counts every call into the wrapped store.
#[derive(Default)]
struct Counted {
    inner: MemoryStore,
    object_reads: AtomicUsize,
    object_writes: AtomicUsize,
    meta_reads: AtomicUsize,
    meta_writes: AtomicUsize,
}

fn tick(n: &AtomicUsize) {
    n.fetch_add(1, Relaxed);
}

#[async_trait]
impl ObjectStore for Counted {
    async fn put_blob(&self, bytes: Bytes) -> scion_store::Result<BlobId> {
        tick(&self.object_writes);
        self.inner.put_blob(bytes).await
    }
    async fn get_blob(&self, id: BlobId) -> scion_store::Result<Option<Bytes>> {
        tick(&self.object_reads);
        self.inner.get_blob(id).await
    }
    async fn has_blobs(&self, ids: &[BlobId]) -> scion_store::Result<Vec<bool>> {
        tick(&self.object_reads);
        self.inner.has_blobs(ids).await
    }
    async fn put_tree(&self, tree: &Tree) -> scion_store::Result<TreeId> {
        tick(&self.object_writes);
        self.inner.put_tree(tree).await
    }
    async fn get_tree(&self, id: TreeId) -> scion_store::Result<Option<Tree>> {
        tick(&self.object_reads);
        self.inner.get_tree(id).await
    }
}

#[async_trait]
impl MetadataStore for Counted {
    async fn put_changeset(&self, cs: &Changeset) -> scion_store::Result<ChangesetId> {
        tick(&self.meta_writes);
        self.inner.put_changeset(cs).await
    }
    async fn get_changeset(
        &self,
        repo: &str,
        id: ChangesetId,
    ) -> scion_store::Result<Option<Changeset>> {
        tick(&self.meta_reads);
        self.inner.get_changeset(repo, id).await
    }
    async fn generation(&self, repo: &str, id: ChangesetId) -> scion_store::Result<Option<u64>> {
        tick(&self.meta_reads);
        self.inner.generation(repo, id).await
    }
    async fn create_ref(
        &self,
        repo: &str,
        name: &str,
        target: ChangesetId,
    ) -> scion_store::Result<RefState> {
        tick(&self.meta_writes);
        self.inner.create_ref(repo, name, target).await
    }
    async fn get_ref(&self, repo: &str, name: &str) -> scion_store::Result<Option<RefState>> {
        tick(&self.meta_reads);
        self.inner.get_ref(repo, name).await
    }
    async fn move_ref(
        &self,
        repo: &str,
        name: &str,
        expected_version: u64,
        new_target: ChangesetId,
    ) -> scion_store::Result<RefState> {
        tick(&self.meta_writes);
        self.inner
            .move_ref(repo, name, expected_version, new_target)
            .await
    }
    async fn list_refs(&self, repo: &str) -> scion_store::Result<Vec<(String, RefState)>> {
        tick(&self.meta_reads);
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
        tick(&self.meta_writes);
        self.inner.put_derived(repo, ty, version, cs, pointer).await
    }
    async fn get_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
    ) -> scion_store::Result<Option<Vec<u8>>> {
        tick(&self.meta_reads);
        self.inner.get_derived(repo, ty, version, cs).await
    }
    async fn put_ingest_checkpoint(
        &self,
        repo: &str,
        git_sha: &[u8],
        cs: ChangesetId,
    ) -> scion_store::Result<()> {
        tick(&self.meta_writes);
        self.inner.put_ingest_checkpoint(repo, git_sha, cs).await
    }
    async fn ingest_checkpoints(
        &self,
        repo: &str,
    ) -> scion_store::Result<Vec<(Vec<u8>, ChangesetId)>> {
        tick(&self.meta_reads);
        self.inner.ingest_checkpoints(repo).await
    }
}

/// Calls into the stores while importing commits `first + 1..=total` of a
/// generated history, after importing the first `first` in an earlier run:
/// the second run starts from what the first stored, as a resumed or
/// incremental import does. Returns (object reads, object writes, metadata
/// reads, metadata writes).
async fn second_run(first: usize, total: usize) -> [usize; 4] {
    let tmp = tempfile::tempdir().unwrap();
    generate(tmp.path(), total);
    let git = GitRepo::open(tmp.path()).unwrap();
    let store = Counted::default();
    let counts = |s: &Counted| {
        [
            s.object_reads.load(Relaxed),
            s.object_writes.load(Relaxed),
            s.meta_reads.load(Relaxed),
            s.meta_writes.load(Relaxed),
        ]
    };

    let rev = format!("main~{}", total - first);
    assert_eq!(
        import(&git, &rev, REPO, &store, &store)
            .await
            .unwrap()
            .created,
        first
    );
    let before = counts(&store);
    let rest = import(&git, "main", REPO, &store, &store).await.unwrap();
    assert_eq!(rest.created, total - first);
    let after = counts(&store);
    std::array::from_fn(|i| after[i] - before[i])
}

#[tokio::test]
async fn each_commit_costs_its_writes_and_a_constant_number_of_reads() {
    let short = second_run(50, 150).await;
    let long = second_run(50, 450).await;
    eprintln!("100 commits: {short:?}, 400 commits: {long:?}");
    let [object_reads, object_writes, meta_reads, meta_writes] = long;

    // What the previous commit wrote is read from memory: only the state
    // the first run left behind is read back, so reads do not grow with the
    // number of commits.
    assert_eq!(object_reads, short[0], "object reads grow with the history");
    assert_eq!(meta_reads, short[2], "metadata reads grow with the history");
    // Per commit: the file's blob, then a tree and a path-history node per
    // directory on its path and the root's.
    assert_eq!(object_writes, 400 * (1 + 2 * (DEPTH + 1)));
    // Per commit: the changeset, its three derived pointers and its
    // checkpoint; then `main` moves once.
    assert_eq!(meta_writes, 400 * 5 + 1);
}
