//! `verify` passes on an honest import and names the path a corrupted
//! mapping gets wrong.

mod common;

use std::collections::BTreeMap;

use bytes::Bytes;
use common::{fixture, git};
use scion_ingest::{GitRepo, IngestError, ObjectId, import, verify, verify_commit};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{BlobId, ChangesetId, FileChange, FileType, RepoPath};

const REPO: &str = "mono";

#[tokio::test]
async fn honest_import_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path());
    let git_repo = GitRepo::open(tmp.path()).unwrap();
    let store = MemoryStore::new();
    common::init(&store, &store, REPO).await;
    import(&git_repo, "main", REPO, &store, &store)
        .await
        .unwrap();

    assert_eq!(
        verify(&git_repo, REPO, &store, &store, None).await.unwrap(),
        5
    );
    assert_eq!(
        verify(&git_repo, REPO, &store, &store, Some(2))
            .await
            .unwrap(),
        2
    );
}

/// A child of `parent` that applies `changes`: what a buggy importer might
/// have stored for the commit `parent` came from.
async fn corrupted(
    store: &MemoryStore,
    parent: ChangesetId,
    changes: BTreeMap<RepoPath, FileChange>,
) -> ChangesetId {
    let mut cs = store.get_changeset(REPO, parent).await.unwrap().unwrap();
    cs.parents = vec![parent];
    cs.file_changes = changes;
    cs.message = "corrupted".into();
    scion_server::create_changesets(store, store, &[cs])
        .await
        .unwrap()[0]
        .0
}

#[tokio::test]
async fn corrupted_mapping_is_reported_at_its_path() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fixture(dir);
    let git_repo = GitRepo::open(dir).unwrap();
    let store = MemoryStore::new();
    common::init(&store, &store, REPO).await;
    let tip = import(&git_repo, "main", REPO, &store, &store)
        .await
        .unwrap()
        .tip;
    let sha = ObjectId::from_hex(git(dir, &["rev-parse", "main"]).trim().as_bytes()).unwrap();
    let p = |s: &str| RepoPath::new(s).unwrap();
    let tracked = |data: &[u8], kind| FileChange::Tracked {
        content: BlobId::hash(data),
        kind,
        size: data.len() as u64,
        copy_from: None,
    };
    store
        .put_blob(Bytes::from_static(b"tampered\n"))
        .await
        .unwrap();

    let cases = [
        (
            BTreeMap::from([(p("README"), tracked(b"tampered\n", FileType::Regular))]),
            "README",
            "content differs",
        ),
        (
            BTreeMap::from([(p("bin/run"), tracked(b"#!/bin/sh\n", FileType::Executable))]),
            "bin/run",
            "file kind differs",
        ),
        (
            BTreeMap::from([(p("side.txt"), FileChange::Deleted)]),
            "side.txt",
            "in git but not in Scion",
        ),
        (
            BTreeMap::from([(p("extra.txt"), tracked(b"tampered\n", FileType::Regular))]),
            "extra.txt",
            "in Scion but not in git",
        ),
    ];
    verify_commit(&git_repo, sha, REPO, tip, &store, &store)
        .await
        .unwrap();
    for (changes, want_path, want_what) in cases {
        let bad = corrupted(&store, tip, changes).await;
        match verify_commit(&git_repo, sha, REPO, bad, &store, &store).await {
            Err(IngestError::Mismatch { commit, path, what }) => {
                assert_eq!((commit, path.as_str(), what), (sha, want_path, want_what));
            }
            other => panic!("expected a mismatch at {want_path}, got {other:?}"),
        }
    }
}
