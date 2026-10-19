//! `MoveRef` over a real transport: creation, fast-forward-only moves and
//! compare-and-swap on the ref version.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use scion_derived::test_util::{REPO, changeset, tracked};
use scion_proto::v1::{GetRefRequest, MoveRefRequest};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{ChangesetId, RepoPath};
use tonic::Code;

async fn commit(store: &MemoryStore, parents: Vec<ChangesetId>, msg: &str) -> ChangesetId {
    let content = msg.as_bytes();
    store
        .put_blob(Bytes::copy_from_slice(content))
        .await
        .unwrap();
    let changes = BTreeMap::from([(RepoPath::new("f.txt").unwrap(), tracked(content))]);
    store
        .put_changeset(&changeset(parents, changes, msg))
        .await
        .unwrap()
}

async fn start(store: Arc<MemoryStore>) -> common::Control {
    let channel = common::serve(store.clone(), store).await;
    common::control(channel)
}

async fn move_ref(
    client: &mut common::Control,
    name: &str,
    target: ChangesetId,
    expected_version: Option<u64>,
) -> Result<u64, Code> {
    client
        .move_ref(MoveRefRequest {
            repo: REPO.into(),
            name: name.into(),
            target: target.as_bytes().to_vec(),
            expected_version,
        })
        .await
        .map(|r| r.into_inner().version)
        .map_err(|s| s.code())
}

#[tokio::test]
async fn creates_then_fast_forwards() {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = commit(&store, vec![], "root").await;
    let a = commit(&store, vec![root], "a").await;
    let b = commit(&store, vec![a], "b").await;
    let mut c = start(store.clone()).await;

    let v0 = move_ref(&mut c, "main", root, None).await.unwrap();
    assert_eq!(
        move_ref(&mut c, "main", a, None).await,
        Err(Code::AlreadyExists)
    );

    // Skipping intermediate changesets is still a fast-forward.
    let v1 = move_ref(&mut c, "main", b, Some(v0)).await.unwrap();
    assert!(v1 > v0);
    let state = store.get_ref(REPO, "main").await.unwrap().unwrap();
    assert_eq!((state.target, state.version), (b, v1));

    let got = c
        .get_ref(GetRefRequest {
            repo: REPO.into(),
            name: "main".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!((got.target, got.version), (b.as_bytes().to_vec(), v1));

    let r = move_ref(&mut c, "release/1.0", a, None).await.unwrap();
    assert_eq!(
        move_ref(&mut c, "release/1.0", b, Some(r)).await.ok(),
        Some(r + 1)
    );
}

#[tokio::test]
async fn rejects_non_fast_forward_and_stale_versions() {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = commit(&store, vec![], "root").await;
    let left = commit(&store, vec![root], "left").await;
    let right = commit(&store, vec![root], "right").await;
    let mut c = start(store.clone()).await;

    let v0 = move_ref(&mut c, "main", left, None).await.unwrap();
    // Sideways and backwards moves are not fast-forwards.
    assert_eq!(
        move_ref(&mut c, "main", right, Some(v0)).await,
        Err(Code::FailedPrecondition)
    );
    assert_eq!(
        move_ref(&mut c, "main", root, Some(v0)).await,
        Err(Code::FailedPrecondition)
    );

    // A version the ref has already moved past loses the race.
    let next = commit(&store, vec![left], "next").await;
    let v1 = move_ref(&mut c, "main", next, Some(v0)).await.unwrap();
    let later = commit(&store, vec![next], "later").await;
    assert_eq!(
        move_ref(&mut c, "main", later, Some(v0)).await,
        Err(Code::FailedPrecondition)
    );
    assert_eq!(
        store.get_ref(REPO, "main").await.unwrap().unwrap().version,
        v1
    );
}

#[tokio::test]
async fn rejects_bad_names_unknown_refs_and_unknown_targets() {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = commit(&store, vec![], "root").await;
    let mut c = start(store.clone()).await;

    for name in ["feature/x", "master", "release/", "users/me"] {
        assert_eq!(
            move_ref(&mut c, name, root, None).await,
            Err(Code::InvalidArgument),
            "{name}"
        );
    }
    assert_eq!(
        move_ref(&mut c, "release/2", root, Some(0)).await,
        Err(Code::NotFound)
    );

    let unknown = ChangesetId::hash(b"never created");
    assert_eq!(
        move_ref(&mut c, "main", unknown, None).await,
        Err(Code::NotFound)
    );
    let v = move_ref(&mut c, "main", root, None).await.unwrap();
    assert_eq!(
        move_ref(&mut c, "main", unknown, Some(v)).await,
        Err(Code::NotFound)
    );

    let err = c
        .move_ref(MoveRefRequest {
            repo: REPO.into(),
            name: "main".into(),
            target: vec![1, 2, 3],
            expected_version: Some(v),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert_eq!(store.list_refs(REPO).await.unwrap().len(), 1);
}
