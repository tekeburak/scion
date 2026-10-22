//! `CommitAndLand`: `CreateChangeset` and a waiting `SubmitChangeRequest` in
//! one round trip, with the path history derived after the reply.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use scion_derived::test_util::{REPO, changeset, tracked};
use scion_derived::{Derivable, PathHistory};
use scion_proto::v1::{
    ChangeRequest, ChangeRequestStatus, CommitAndLandRequest, GetRefRequest,
    ListChangeRequestsRequest, PathLogRequest,
};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{Changeset, ChangesetId, Encode, RepoPath};
use tonic::Code;

/// A repository whose `main` is at an empty root.
async fn repository(store: &MemoryStore) -> ChangesetId {
    let root = store
        .put_changeset(&changeset(vec![], BTreeMap::new(), "root"))
        .await
        .unwrap();
    store.create_ref(REPO, "main", root).await.unwrap();
    root
}

/// A child of `parent` writing `text` to `path`, its blob uploaded.
async fn edit(store: &MemoryStore, parent: ChangesetId, path: &str, text: &str) -> Changeset {
    store
        .put_blob(Bytes::copy_from_slice(text.as_bytes()))
        .await
        .unwrap();
    let changes = BTreeMap::from([(RepoPath::new(path).unwrap(), tracked(text.as_bytes()))]);
    changeset(vec![parent], changes, text)
}

async fn commit_and_land(c: &mut common::Control, cs: &Changeset) -> ChangeRequest {
    c.commit_and_land(CommitAndLandRequest {
        changeset: cs.encode_to_vec(),
    })
    .await
    .unwrap()
    .into_inner()
}

async fn main_target(c: &mut common::Control) -> Vec<u8> {
    c.get_ref(GetRefRequest {
        repo: REPO.into(),
        name: "main".into(),
    })
    .await
    .unwrap()
    .into_inner()
    .target
}

/// Wait until `id`'s path history is stored.
async fn history_stored(store: &dyn MetadataStore, id: ChangesetId) {
    for _ in 0..500 {
        let stored = store
            .get_derived(REPO, PathHistory::NAME, PathHistory::VERSION, id)
            .await
            .unwrap();
        if stored.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the path history of {id} was never stored");
}

#[tokio::test]
async fn lands_in_one_call_and_stores_the_history_after() {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = repository(&store).await;
    let mut c = common::control(common::serve(store.clone(), store.clone()).await);

    let cs = edit(&store, root, "a.txt", "a\n").await;
    let cr = commit_and_land(&mut c, &cs).await;
    assert_eq!(cr.status(), ChangeRequestStatus::Landed);
    assert_eq!(cr.head, cs.id().as_bytes());
    assert_eq!(cr.base, root.as_bytes());
    assert_eq!(cr.landed.as_deref(), Some(cs.id().as_bytes().as_slice()));
    assert_eq!(main_target(&mut c).await, cs.id().as_bytes());

    history_stored(&*store, cs.id()).await;
    let log = c
        .path_log(PathLogRequest {
            repo: REPO.into(),
            head: cs.id().as_bytes().to_vec(),
            path: "a.txt".into(),
            limit: 10,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(log.changesets, vec![cs.encode_to_vec()]);
}

#[tokio::test]
async fn a_conflict_keeps_the_changeset_and_main() {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = repository(&store).await;
    let mut c = common::control(common::serve(store.clone(), store.clone()).await);

    let first = edit(&store, root, "a.txt", "one\n").await;
    commit_and_land(&mut c, &first).await;
    let second = edit(&store, root, "a.txt", "two\n").await;
    let cr = commit_and_land(&mut c, &second).await;
    assert_eq!(cr.status(), ChangeRequestStatus::NeedsRebase);
    assert_eq!(cr.conflicts, ["a.txt"]);
    assert_eq!(main_target(&mut c).await, first.id().as_bytes());
    assert!(
        store
            .get_changeset(REPO, second.id())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_changeset_create_refuses_submits_nothing() {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = repository(&store).await;
    let mut c = common::control(common::serve(store.clone(), store.clone()).await);

    // The blob was never uploaded.
    let changes = BTreeMap::from([(RepoPath::new("a.txt").unwrap(), tracked(b"a\n"))]);
    let cs = changeset(vec![root], changes, "no blob");
    let err = c
        .commit_and_land(CommitAndLandRequest {
            changeset: cs.encode_to_vec(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition, "{err}");
    let requests = c
        .list_change_requests(ListChangeRequestsRequest {
            repo: REPO.into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
        .requests;
    assert!(requests.is_empty(), "{requests:?}");
    assert_eq!(main_target(&mut c).await, root.as_bytes());
}
