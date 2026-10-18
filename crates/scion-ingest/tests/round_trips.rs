//! Store round trips per imported commit are bounded by the change, not by
//! the history: what one commit costs on Postgres and S3 stays the same from
//! the first commits to the last.

mod common;

use std::io::Write;
use std::ops::Range;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use common::git;
use futures_util::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore as Backend, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use scion_ingest::{GitRepo, import};
use scion_store::{
    ChangeRequest, ChangeRequestStatus, DerivedPointer, LandOutcome, MemoryStore, MetadataStore,
    ObjectStorage, RefState,
};
use scion_types::{Changeset, ChangesetId};

const REPO: &str = "trips";
const COMMITS: usize = 600;
/// Commits per measured window.
const WINDOW: usize = 100;
/// Object-store requests one single-file commit at depth 2 costs: one
/// presence check and one upload for its blob, a tree for each of the two
/// directories on its path, and one page of path-history nodes. Re-reading
/// what the parent wrote, and checking the blob again on create, are free.
const MAX_OBJECT_REQUESTS: usize = 5;
/// Metadata-store calls one commit may cost, the batch's shared calls
/// (changesets, derived pointers, checkpoints) included: the reads that find
/// its derived values absent, its changeset, the parent's values and the skip
/// list's log-many ancestors. Postgres answers the write-once ones from
/// memory, so this bounds its round trips from above.
const MAX_METADATA_CALLS: usize = 20;

/// A linear history where commit `i` rewrites one of 500 files in 20
/// directories, written with `git fast-import`.
fn generate(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    let mut stream = Vec::new();
    for i in 1..=COMMITS {
        let body = format!("line one\nversion {i}\n");
        writeln!(stream, "commit refs/heads/main").unwrap();
        writeln!(
            stream,
            "committer Gen <gen@example.com> {} +0000",
            1_700_000_000 + i
        )
        .unwrap();
        writeln!(stream, "data 2\nc\n").unwrap();
        writeln!(stream, "M 100644 inline dir{}/file{}.txt", i % 20, i % 500).unwrap();
        writeln!(stream, "data {}\n{body}", body.len()).unwrap();
    }
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("git is installed");
    child.stdin.take().unwrap().write_all(&stream).unwrap();
    assert!(child.wait().unwrap().success());
}

/// Counts every request that reaches the object-store backend.
#[derive(Debug)]
struct CountingBackend {
    inner: InMemory,
    requests: Arc<AtomicUsize>,
}

impl std::fmt::Display for CountingBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CountingBackend")
    }
}

#[async_trait]
impl Backend for CountingBackend {
    async fn put_opts(
        &self,
        location: &Key,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.put_opts(location, payload, opts).await
    }
    async fn put_multipart_opts(
        &self,
        location: &Key,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.put_multipart_opts(location, opts).await
    }
    async fn get_opts(
        &self,
        location: &Key,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.get_opts(location, options).await
    }
    async fn get_ranges(
        &self,
        location: &Key,
        ranges: &[Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.get_ranges(location, ranges).await
    }
    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Key>>,
    ) -> BoxStream<'static, object_store::Result<Key>> {
        self.inner.delete_stream(locations)
    }
    fn list(&self, prefix: Option<&Key>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Key>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &Key,
        to: &Key,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// Counts every metadata call.
struct CountingMeta {
    inner: MemoryStore,
    calls: AtomicUsize,
}

impl CountingMeta {
    fn tick(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}

#[async_trait]
impl MetadataStore for CountingMeta {
    async fn create_repo(&self, name: &str) -> scion_store::Result<()> {
        self.tick();
        self.inner.create_repo(name).await
    }
    async fn put_changesets(&self, batch: &[Changeset]) -> scion_store::Result<()> {
        self.tick();
        self.inner.put_changesets(batch).await
    }
    async fn get_changeset(
        &self,
        repo: &str,
        id: ChangesetId,
    ) -> scion_store::Result<Option<Changeset>> {
        self.tick();
        self.inner.get_changeset(repo, id).await
    }
    async fn generation(&self, repo: &str, id: ChangesetId) -> scion_store::Result<Option<u64>> {
        self.tick();
        self.inner.generation(repo, id).await
    }
    async fn create_ref(
        &self,
        repo: &str,
        name: &str,
        target: ChangesetId,
    ) -> scion_store::Result<RefState> {
        self.tick();
        self.inner.create_ref(repo, name, target).await
    }
    async fn get_ref(&self, repo: &str, name: &str) -> scion_store::Result<Option<RefState>> {
        self.tick();
        self.inner.get_ref(repo, name).await
    }
    async fn move_ref(
        &self,
        repo: &str,
        name: &str,
        expected_version: u64,
        new_target: ChangesetId,
    ) -> scion_store::Result<RefState> {
        self.tick();
        self.inner
            .move_ref(repo, name, expected_version, new_target)
            .await
    }
    async fn list_refs(&self, repo: &str) -> scion_store::Result<Vec<(String, RefState)>> {
        self.tick();
        self.inner.list_refs(repo).await
    }
    async fn put_derived_many(
        &self,
        repo: &str,
        pointers: Vec<DerivedPointer>,
    ) -> scion_store::Result<()> {
        self.tick();
        self.inner.put_derived_many(repo, pointers).await
    }
    async fn get_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
    ) -> scion_store::Result<Option<Vec<u8>>> {
        self.tick();
        self.inner.get_derived(repo, ty, version, cs).await
    }
    async fn put_ingest_checkpoints(
        &self,
        repo: &str,
        done: &[(Vec<u8>, ChangesetId)],
    ) -> scion_store::Result<()> {
        self.tick();
        self.inner.put_ingest_checkpoints(repo, done).await
    }
    async fn ingest_checkpoints(
        &self,
        repo: &str,
    ) -> scion_store::Result<Vec<(Vec<u8>, ChangesetId)>> {
        self.tick();
        self.inner.ingest_checkpoints(repo).await
    }

    async fn submit_change_request(
        &self,
        repo: &str,
        head: ChangesetId,
        base: ChangesetId,
    ) -> scion_store::Result<ChangeRequest> {
        self.tick();
        self.inner.submit_change_request(repo, head, base).await
    }

    async fn get_change_request(
        &self,
        repo: &str,
        id: u64,
    ) -> scion_store::Result<Option<ChangeRequest>> {
        self.tick();
        self.inner.get_change_request(repo, id).await
    }

    async fn list_change_requests(
        &self,
        repo: &str,
        status: Option<ChangeRequestStatus>,
        limit: u32,
    ) -> scion_store::Result<Vec<ChangeRequest>> {
        self.tick();
        self.inner.list_change_requests(repo, status, limit).await
    }

    async fn find_landings(
        &self,
        repo: &str,
        changesets: &[ChangesetId],
    ) -> scion_store::Result<Vec<(ChangesetId, ChangeRequest)>> {
        self.tick();
        self.inner.find_landings(repo, changesets).await
    }

    async fn claim_next_change_request(
        &self,
        repo: &str,
    ) -> scion_store::Result<Option<ChangeRequest>> {
        self.tick();
        self.inner.claim_next_change_request(repo).await
    }

    async fn landing_change_request(
        &self,
        repo: &str,
    ) -> scion_store::Result<Option<ChangeRequest>> {
        self.tick();
        self.inner.landing_change_request(repo).await
    }

    async fn finish_change_request(
        &self,
        repo: &str,
        id: u64,
        outcome: LandOutcome,
    ) -> scion_store::Result<ChangeRequest> {
        self.tick();
        self.inner.finish_change_request(repo, id, outcome).await
    }

    async fn repos_with_open_change_requests(&self) -> scion_store::Result<Vec<String>> {
        self.tick();
        self.inner.repos_with_open_change_requests().await
    }
}

#[tokio::test]
async fn round_trips_per_commit_do_not_grow_with_history() {
    let tmp = tempfile::tempdir().unwrap();
    generate(tmp.path());
    let git = GitRepo::open(tmp.path()).unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let objects = ObjectStorage::new(Arc::new(CountingBackend {
        inner: InMemory::new(),
        requests: requests.clone(),
    }));
    let meta = CountingMeta {
        inner: MemoryStore::new(),
        calls: AtomicUsize::new(0),
    };
    common::init(&meta, &objects, REPO).await;

    // Per-commit cost of importing up to `rev`, from where the last call
    // stopped. The fixed per-import calls (checkpoints, `main`) are a few,
    // spread over a whole window.
    let window = async |rev: String| {
        let (r0, m0) = (
            requests.load(Ordering::Relaxed),
            meta.calls.load(Ordering::Relaxed),
        );
        let done = import(&git, &rev, REPO, &meta, &objects).await.unwrap();
        assert_eq!(done.created, WINDOW);
        let r = (requests.load(Ordering::Relaxed) - r0) / WINDOW;
        let m = (meta.calls.load(Ordering::Relaxed) - m0) / WINDOW;
        eprintln!("{rev}: {r} object requests, {m} metadata calls per commit");
        (r, m)
    };
    window(format!("main~{}", COMMITS - WINDOW)).await;
    let early = window(format!("main~{}", COMMITS - 2 * WINDOW)).await;
    for rev in (1..COMMITS / WINDOW - 2).rev() {
        window(format!("main~{}", rev * WINDOW)).await;
    }
    let late = window("main".to_owned()).await;

    for (objects, metadata) in [early, late] {
        assert!(
            objects <= MAX_OBJECT_REQUESTS,
            "{objects} object requests per commit"
        );
        assert!(
            metadata <= MAX_METADATA_CALLS,
            "{metadata} metadata calls per commit"
        );
    }
    assert!(
        late.0 <= early.0,
        "object requests grew: {early:?} → {late:?}"
    );
}
