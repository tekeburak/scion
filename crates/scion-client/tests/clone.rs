//! Clone against an in-process `scion-server` backed by a memory store.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use scion_client::{CheckoutError, Remote, RemoteError, SparseProfile, Workspace, clone};
use scion_derived::test_util::{REPO, changeset};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{BlobId, ChangesetId, FileChange, FileType, RepoPath};
use tokio::net::TcpListener;

/// The key the test server signs tokens with.
fn key() -> scion_server::TokenKey {
    scion_server::TokenKey::from_hex(&"5c".repeat(32)).unwrap()
}

/// A token of [`key`], valid for an hour.
fn token() -> String {
    let hour = std::time::Duration::from_secs(3600);
    scion_server::token(&key(), "sp:test", hour, std::time::SystemTime::now()).unwrap()
}

/// Make `sp:test`, the principal of [`token`], an admin of the server and of
/// `repo` if it exists.
async fn admit(meta: &dyn scion_store::MetadataStore, repo: &str) {
    let admin = scion_store::Grant {
        principal: "sp:test".into(),
        path_prefix: String::new(),
        role: scion_store::Role::Admin,
    };
    meta.put_grant(scion_store::SERVER_GRANTS, &admin)
        .await
        .unwrap();
    let _ = meta.put_grant(repo, &admin).await;
}

const FILES: &[(&str, &[u8], FileType)] = &[
    ("README", b"hello\n", FileType::Regular),
    ("bin/run", b"#!/bin/sh\necho hi\n", FileType::Executable),
    ("docs/link", b"../README", FileType::Symlink),
    ("srv/api/main.rs", b"fn main() {}\n", FileType::Regular),
    (
        "srv/api/lib/util.rs",
        b"pub fn util() {}\n",
        FileType::Regular,
    ),
    ("srv/web/index.html", b"<html></html>\n", FileType::Regular),
];

/// A server whose `main` holds [`FILES`]; returns its address and `main`.
async fn serve() -> (String, ChangesetId) {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let mut changes = BTreeMap::new();
    for (path, content, kind) in FILES {
        store.put_blob(Bytes::from_static(content)).await.unwrap();
        changes.insert(
            RepoPath::new(*path).unwrap(),
            FileChange::Tracked {
                content: BlobId::hash(content),
                kind: *kind,
                size: content.len() as u64,
                copy_from: None,
            },
        );
    }
    let id = store
        .put_changeset(&changeset(vec![], changes, "root"))
        .await
        .unwrap();
    store.create_ref(REPO, "main", id).await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    admit(&*store, REPO).await;
    tokio::spawn(scion_server::serve(
        listener,
        store.clone(),
        store,
        key(),
        None,
    ));
    (format!("http://{addr}"), id)
}

/// A connection to `server` with [`token`].
async fn remote(server: &str) -> Remote {
    Remote::with_token(server, &token()).await.unwrap()
}

/// Every file under `root` outside `.scn/`, relative and sorted.
fn listing(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_name() == ".scn" {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap();
                out.push(rel.to_str().unwrap().to_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[tokio::test]
async fn full_clone_materializes_every_file() {
    let (server, main) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("wc");
    let ws = clone(
        remote(&server).await,
        &server,
        REPO,
        &dest,
        SparseProfile::full(),
    )
    .await
    .unwrap();

    let mut expected: Vec<&str> = FILES.iter().map(|(p, ..)| *p).collect();
    expected.sort_unstable();
    assert_eq!(listing(&dest), expected);
    assert_eq!(
        std::fs::read(dest.join("srv/api/main.rs")).unwrap(),
        b"fn main() {}\n"
    );
    let mode = std::fs::metadata(dest.join("bin/run"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "executable bit set");
    assert_eq!(
        std::fs::read_link(dest.join("docs/link")).unwrap(),
        Path::new("../README")
    );

    let state = ws.read_state().unwrap();
    assert_eq!((state.current_head, state.base), (main, main));
    let config = ws.read_config().unwrap();
    assert_eq!(
        (config.server.as_str(), config.repo.as_str()),
        (server.as_str(), REPO)
    );

    let index = ws.read_index().unwrap();
    assert_eq!(index.entries.len(), FILES.len());
    for (path, content, kind) in FILES {
        let e = &index.entries[&RepoPath::new(*path).unwrap()];
        assert_eq!(
            (e.id, e.kind, e.size),
            (BlobId::hash(content), *kind, content.len() as u64)
        );
    }

    // The working copy is found from any directory inside it.
    let found = Workspace::find(&dest.join("srv/api")).unwrap();
    assert_eq!(found.root(), dest);
}

#[tokio::test]
async fn sparse_clone_materializes_only_the_profile() {
    let (server, _) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let sparse = SparseProfile::new(vec![
        RepoPath::new("srv/api").unwrap(),
        RepoPath::new("README").unwrap(),
    ]);
    let ws = clone(
        remote(&server).await,
        &server,
        REPO,
        dir.path(),
        sparse.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        listing(dir.path()),
        ["README", "srv/api/lib/util.rs", "srv/api/main.rs"]
    );
    assert_eq!(ws.read_sparse().unwrap(), sparse);
    assert_eq!(ws.read_index().unwrap().entries.len(), 3);
}

#[tokio::test]
async fn refuses_non_empty_destinations_and_unknown_repos() {
    let (server, _) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stray"), b"x").unwrap();
    assert!(matches!(
        clone(
            remote(&server).await,
            &server,
            REPO,
            dir.path(),
            SparseProfile::full()
        )
        .await,
        Err(CheckoutError::NotEmpty(_))
    ));

    let empty = tempfile::tempdir().unwrap();
    match clone(
        remote(&server).await,
        &server,
        "no-such-repo",
        empty.path(),
        SparseProfile::full(),
    )
    .await
    {
        Err(CheckoutError::Remote(RemoteError::Rpc(status))) => {
            // Nobody holds a role in a repository that does not exist.
            assert_eq!(status.code(), tonic::Code::PermissionDenied);
        }
        other => panic!("expected PERMISSION_DENIED, got {other:?}"),
    }
    assert!(!empty.path().join(".scn").exists(), "nothing written");
}
