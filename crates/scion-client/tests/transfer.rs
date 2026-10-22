//! How many RPCs a clone and a commit make, against an in-process
//! `scion-server` whose every request is counted.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use scion_client::{Remote, RequestStatus, SparseProfile, clone, commit, commit_and_land};
use scion_derived::test_util::{REPO, changeset};
use scion_store::{Grant, MemoryStore, MetadataStore, ObjectStore, Role, SERVER_GRANTS};
use scion_types::{BlobId, FileChange, FileType, Identity, RepoPath, Timestamp};
use tokio::net::TcpListener;
use tonic::service::InterceptorLayer;
use tonic::transport::server::TcpIncoming;

fn key() -> scion_server::TokenKey {
    scion_server::TokenKey::from_hex(&"5c".repeat(32)).unwrap()
}

fn token() -> String {
    let hour = std::time::Duration::from_secs(3600);
    scion_server::token(&key(), "sp:test", hour, std::time::SystemTime::now()).unwrap()
}

/// The `i`th file: nine directories deep at most, a few hundred directories
/// in all, like the benchmark's slice.
fn path(i: usize) -> RepoPath {
    let mut path = String::new();
    let mut n = i;
    for depth in 0..(i % 9) {
        write!(path, "d{depth}_{}/", n % 4).unwrap();
        n /= 4;
    }
    RepoPath::new(format!("{path}f{i}.rs")).unwrap()
}

fn content(i: usize) -> Vec<u8> {
    format!("// file {i}\n").into_bytes()
}

/// A server whose `main` holds `files` files, counting every request it
/// receives; returns its address and the counter.
async fn serve(files: usize) -> (String, Arc<AtomicUsize>) {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let mut changes = BTreeMap::new();
    for i in 0..files {
        let data = content(i);
        store.put_blob(Bytes::from(data.clone())).await.unwrap();
        changes.insert(
            path(i),
            FileChange::Tracked {
                content: BlobId::hash(&data),
                kind: FileType::Regular,
                size: data.len() as u64,
                copy_from: None,
            },
        );
    }
    let id = store
        .put_changeset(&changeset(vec![], changes, "root"))
        .await
        .unwrap();
    store.create_ref(REPO, "main", id).await.unwrap();
    let admin = Grant {
        principal: "sp:test".into(),
        path_prefix: String::new(),
        role: Role::Admin,
    };
    store.put_grant(SERVER_GRANTS, &admin).await.unwrap();
    store.put_grant(REPO, &admin).await.unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let count = InterceptorLayer::new(move |req| {
        counter.fetch_add(1, Ordering::Relaxed);
        Ok(req)
    });
    let routes = scion_server::routes(store.clone(), store, key(), None)
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .layer(count)
            .add_routes(routes)
            .serve_with_incoming(TcpIncoming::from(listener)),
    );
    (format!("http://{addr}"), calls)
}

#[tokio::test]
async fn a_2330_file_clone_makes_at_most_12_rpcs() {
    let (server, calls) = serve(2330).await;
    let dir = tempfile::tempdir().unwrap();
    let remote = Remote::with_token(&server, &token()).await.unwrap();
    let ws = clone(remote, &server, REPO, dir.path(), SparseProfile::full())
        .await
        .unwrap();
    // GetRef, every tree in one GetObjects, the blobs 256 per GetObjects.
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1 + 1 + 2330_usize.div_ceil(256)
    );
    assert!(calls.load(Ordering::Relaxed) <= 12);
    let index = ws.read_index().unwrap();
    assert_eq!(index.entries.len(), 2330);
    for i in [0, 8, 1000, 2329] {
        assert_eq!(
            std::fs::read(dir.path().join(path(i).as_str())).unwrap(),
            content(i)
        );
    }

    // A sparse clone fetches the trees on the way to its paths one level at
    // a time, and each listed directory with everything beneath it at once.
    calls.store(0, Ordering::Relaxed);
    let sparse_dir = tempfile::tempdir().unwrap();
    let sparse = SparseProfile::new(vec![RepoPath::new("d0_1/d1_2").unwrap()]);
    let remote = Remote::with_token(&server, &token()).await.unwrap();
    let ws = clone(remote, &server, REPO, sparse_dir.path(), sparse.clone())
        .await
        .unwrap();
    let files = (0..2330).filter(|&i| sparse.includes(&path(i))).count();
    assert!(files > 0);
    assert_eq!(ws.read_index().unwrap().entries.len(), files);
    // GetRef, the root, `d0_1`, `d0_1/d1_2` with its subtrees, the blobs.
    assert_eq!(calls.load(Ordering::Relaxed), 1 + 3 + files.div_ceil(256));
}

#[tokio::test]
async fn commit_uploads_only_the_blobs_the_server_lacks() {
    let (server, calls) = serve(600).await;
    let dir = tempfile::tempdir().unwrap();
    let remote = Remote::with_token(&server, &token()).await.unwrap();
    let ws = clone(
        remote.clone(),
        &server,
        REPO,
        dir.path(),
        SparseProfile::full(),
    )
    .await
    .unwrap();
    let author = Identity::new("a", "a@x");

    // 300 new contents: two `HasBlobs`, two `PutBlobs`, `CreateChangeset`.
    for i in 0..300 {
        std::fs::write(dir.path().join(path(i).as_str()), format!("new {i}\n")).unwrap();
    }
    calls.store(0, Ordering::Relaxed);
    commit(&ws, &remote, author.clone(), Timestamp::new(1, 0), "edit")
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 5);

    // Back to contents the server already has: no `PutBlobs` at all.
    for i in 0..300 {
        std::fs::write(dir.path().join(path(i).as_str()), content(i)).unwrap();
    }
    calls.store(0, Ordering::Relaxed);
    let id = commit(&ws, &remote, author, Timestamp::new(2, 0), "revert")
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 3);

    // The server holds every file of the result.
    let files = remote.clone().diff(REPO, None, id).await.unwrap();
    assert_eq!(files.len(), 600);
    for f in files {
        let blob = f.new.unwrap().id;
        assert!(remote.clone().has_blobs(REPO, &[blob]).await.unwrap()[0]);
    }
}

/// Phase 10.6: a one-file commit that lands is `HasBlobs`, `PutBlobs` and
/// `CommitAndLand`, three RPCs, where `commit` and `land` made five.
#[tokio::test]
async fn commit_and_land_makes_three_rpcs() {
    let (server, calls) = serve(10).await;
    let dir = tempfile::tempdir().unwrap();
    let remote = Remote::with_token(&server, &token()).await.unwrap();
    let ws = clone(
        remote.clone(),
        &server,
        REPO,
        dir.path(),
        SparseProfile::full(),
    )
    .await
    .unwrap();
    std::fs::write(dir.path().join(path(3).as_str()), "edited\n").unwrap();
    calls.store(0, Ordering::Relaxed);
    let cr = commit_and_land(
        &ws,
        &remote,
        Identity::new("a", "a@x"),
        Timestamp::new(1, 0),
        "edit",
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    assert_eq!(cr.status, RequestStatus::Landed(cr.head));
    assert_eq!(ws.read_state().unwrap().current_head, cr.head);
    let main = remote.clone().get_ref(REPO, "main").await.unwrap();
    assert_eq!(main.target, cr.head);
}
