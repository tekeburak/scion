//! `Revert`, the server half of `scn revert`: undoing a changeset restores
//! the tree it started from, keeps blame on the original changesets, and is
//! refused when a path it changed was changed again since.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use scion_derived::test_util::{REPO, changeset, tracked};
use scion_derived::{Blame, Deriver, RootTree};
use scion_proto::v1::RevertRequest;
use scion_server::{Reverted, revert};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{ChangesetId, FileChange, Identity, RepoPath, Timestamp};
use tonic::Code;

fn path(p: &str) -> RepoPath {
    RepoPath::new(p).unwrap()
}

/// Create a changeset on `parent` through the shared create path; `None`
/// content deletes the path.
async fn create(
    store: &MemoryStore,
    parent: Option<ChangesetId>,
    files: &[(&str, Option<&str>)],
    msg: &str,
) -> ChangesetId {
    let mut changes = BTreeMap::new();
    for (p, content) in files {
        let change = match content {
            Some(content) => {
                store
                    .put_blob(Bytes::copy_from_slice(content.as_bytes()))
                    .await
                    .unwrap();
                tracked(content.as_bytes())
            }
            None => FileChange::Deleted,
        };
        changes.insert(path(p), change);
    }
    let cs = changeset(parent.into_iter().collect(), changes, msg);
    scion_server::create_changesets(store, store, &[cs])
        .await
        .unwrap()[0]
        .0
}

fn author() -> (Identity, Timestamp) {
    (Identity::new("R", "r@example.com"), Timestamp::new(1, 0))
}

async fn tree(store: &MemoryStore, cs: ChangesetId) -> RootTree {
    Deriver::new(REPO, store, store)
        .derive::<RootTree>(cs)
        .await
        .unwrap()
}

async fn undo(store: &MemoryStore, head: ChangesetId, target: ChangesetId) -> ChangesetId {
    match revert(store, store, REPO, (head, target), author(), "revert")
        .await
        .unwrap()
    {
        Reverted::Clean { reverted, root } => {
            assert_eq!(root, tree(store, reverted).await.0);
            reverted
        }
        other @ Reverted::ChangedSince { .. } => panic!("{other:?}"),
    }
}

/// `base` has `f`, `h` and a file `d`; `change` edits `f`, adds `g`,
/// deletes `h` and turns `d` into a directory; `later` adds `k`.
async fn history() -> (MemoryStore, [ChangesetId; 3]) {
    let store = MemoryStore::with_repo(REPO);
    let base = create(
        &store,
        None,
        &[
            ("f", Some("1\n2\n3\n")),
            ("h", Some("h\n")),
            ("d", Some("d\n")),
        ],
        "base",
    )
    .await;
    let change = create(
        &store,
        Some(base),
        &[
            ("f", Some("1\ntwo\n3\n")),
            ("g", Some("g\n")),
            ("h", None),
            ("d", None),
            ("d/x", Some("x\n")),
        ],
        "change",
    )
    .await;
    let later = create(&store, Some(change), &[("k", Some("k\n"))], "later").await;
    (store, [base, change, later])
}

#[tokio::test]
async fn reverting_restores_the_tree_and_the_blame() {
    let (store, [base, change, later]) = history().await;

    // Reverting the head itself gives back its parent's tree exactly.
    let reverted = undo(&store, change, change).await;
    assert_eq!(tree(&store, reverted).await, tree(&store, base).await);

    // Under a later change, only `change`'s paths move back.
    let reverted = undo(&store, later, change).await;
    let cs = store.get_changeset(REPO, reverted).await.unwrap().unwrap();
    assert_eq!(cs.parents, [later]);
    assert_eq!(cs.message, "revert");
    let names: Vec<&str> = cs.file_changes.keys().map(RepoPath::as_str).collect();
    assert_eq!(names, ["d", "d/x", "f", "g", "h"]);
    let FileChange::Tracked { copy_from, .. } = &cs.file_changes[&path("f")] else {
        panic!("tracked")
    };
    assert_eq!(copy_from, &Some((path("f"), base)));
    let d = Deriver::new(REPO, &store, &store);
    assert_eq!(
        Blame::for_path(&d, reverted, &path("f"))
            .await
            .unwrap()
            .unwrap()
            .runs,
        [(base, 3)]
    );
    // Reverting `later` too is the base tree again.
    let both = undo(&store, reverted, later).await;
    assert_eq!(tree(&store, both).await, tree(&store, base).await);

    // A revert is a changeset like any other: reverting it redoes `change`.
    let redone = undo(&store, reverted, reverted).await;
    assert_eq!(tree(&store, redone).await, tree(&store, later).await);
}

#[tokio::test]
async fn a_path_changed_since_is_refused() {
    let (store, [base, change, later]) = history().await;
    let edit = create(&store, Some(later), &[("f", Some("1\n2\n3\n4\n"))], "edit").await;
    assert_eq!(
        revert(&store, &store, REPO, (edit, change), author(), "r")
            .await
            .unwrap(),
        Reverted::ChangedSince {
            paths: vec![path("f")]
        }
    );
    // Not in the head's history, or nothing to undo.
    let side = create(&store, Some(base), &[("s", Some("s\n"))], "side").await;
    let empty = store
        .put_changeset(&changeset(vec![later], BTreeMap::new(), "empty"))
        .await
        .unwrap();
    for (head, target) in [(side, change), (empty, empty)] {
        let err = revert(&store, &store, REPO, (head, target), author(), "r")
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::FailedPrecondition, "{err}");
    }
}

#[tokio::test]
async fn unknown_changesets_are_not_found() {
    let (store, [_, change, _]) = history().await;
    let store = Arc::new(store);
    let channel = common::serve(store.clone(), store).await;
    let mut client = common::control(channel);
    let unknown = ChangesetId::hash(b"unknown");
    for (repo, head, target) in [
        (REPO, change, unknown),
        (REPO, unknown, change),
        ("other", change, change),
    ] {
        let err = client
            .revert(RevertRequest {
                repo: repo.into(),
                head: head.as_bytes().to_vec(),
                changeset: target.as_bytes().to_vec(),
                message: "r".into(),
                ..RevertRequest::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::NotFound, "{repo} {head} {target}");
    }
}
