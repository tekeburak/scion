//! A grant on a directory: a writer changes only paths under it, however the
//! change arrives (a changeset, a merge, a revert, a rebase or a change
//! request the land worker rebases), and a reader reads only those paths.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use common::{Bearer, Control};
use scion_derived::test_util::{REPO, changeset, tracked};
use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::{
    BlameRequest, ChangeRequestStatus, CreateChangesetRequest, DiffRequest,
    GetChangeRequestRequest, GetRefRequest, MergeRequest, MoveRefRequest, PathLogRequest,
    RebaseRequest, RevertRequest, SubmitChangeRequestRequest,
};
use scion_store::{Grant, MemoryStore, MetadataStore, ObjectStore, Role};
use scion_types::{ChangesetId, Encode, RepoPath};
use tonic::{Code, Status};

/// A child of `parent` writing its path as the content of each of `paths`,
/// stored directly, as another writer would have created it.
async fn put(store: &MemoryStore, parent: ChangesetId, paths: &[&str], msg: &str) -> ChangesetId {
    let mut changes = BTreeMap::new();
    for path in paths {
        let content = format!("{path} {msg}\n").into_bytes();
        store.put_blob(Bytes::from(content.clone())).await.unwrap();
        changes.insert(RepoPath::new(*path).unwrap(), tracked(&content));
    }
    store
        .put_changeset(&changeset(vec![parent], changes, msg))
        .await
        .unwrap()
}

/// `main`, whose files are `lib/a` and `app/b`, and a server where `sp:lib`
/// writes under `lib/` and `sp:look` reads under `lib/`.
async fn setup() -> (Arc<MemoryStore>, tonic::transport::Channel, ChangesetId) {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = store
        .put_changeset(&changeset(vec![], BTreeMap::new(), "root"))
        .await
        .unwrap();
    let main = put(&store, root, &["lib/a", "app/b"], "base").await;
    store.create_ref(REPO, "main", main).await.unwrap();
    for (principal, role) in [("sp:lib", Role::Writer), ("sp:look", Role::Reader)] {
        let grant = Grant {
            principal: principal.to_owned(),
            path_prefix: "lib".to_owned(),
            role,
        };
        store.put_grant(REPO, &grant).await.unwrap();
    }
    let channel = common::serve(store.clone(), store.clone()).await;
    (store, channel, main)
}

fn client(channel: &tonic::transport::Channel, principal: &str) -> Control {
    let token = common::key()
        .issue(principal, SystemTime::now() + Duration::from_secs(3600))
        .unwrap();
    ControlServiceClient::with_interceptor(channel.clone(), Bearer::of(&token))
}

/// The error is `PERMISSION_DENIED` and names `path`.
fn denied<T: std::fmt::Debug>(result: Result<T, Status>, path: &str) {
    let err = result.unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied, "{err}");
    assert!(
        err.message().contains(&format!(" {path}:")),
        "names {path}: {err}"
    );
}

async fn create(
    c: &mut Control,
    store: &MemoryStore,
    parent: ChangesetId,
    path: &str,
) -> Result<ChangesetId, Status> {
    let content = format!("{path} new\n").into_bytes();
    store.put_blob(Bytes::from(content.clone())).await.unwrap();
    let changes = BTreeMap::from([(RepoPath::new(path).unwrap(), tracked(&content))]);
    let cs = changeset(vec![parent], changes, path);
    let resp = c
        .create_changeset(CreateChangesetRequest {
            changeset: cs.encode_to_vec(),
        })
        .await?
        .into_inner();
    Ok(ChangesetId::try_from_slice(&resp.id).unwrap())
}

async fn submit(c: &mut Control, head: ChangesetId) -> scion_proto::v1::ChangeRequest {
    let cr = c
        .submit_change_request(SubmitChangeRequestRequest {
            repo: REPO.into(),
            head: head.as_bytes().to_vec(),
            wait: true,
        })
        .await
        .unwrap()
        .into_inner();
    c.get_change_request(GetChangeRequestRequest {
        repo: REPO.into(),
        id: cr.id,
        wait: true,
    })
    .await
    .unwrap()
    .into_inner()
}

async fn main_at(c: &mut Control) -> ChangesetId {
    let main = c
        .get_ref(GetRefRequest {
            repo: REPO.into(),
            name: "main".into(),
        })
        .await
        .unwrap()
        .into_inner();
    ChangesetId::try_from_slice(&main.target).unwrap()
}

#[tokio::test]
async fn a_writer_lands_under_its_directory_only() {
    let (store, channel, main) = setup().await;
    let mut lib = client(&channel, "sp:lib");

    let mine = create(&mut lib, &store, main, "lib/a").await.unwrap();
    let cr = submit(&mut lib, mine).await;
    assert_eq!(cr.status(), ChangeRequestStatus::Landed);
    assert_eq!(main_at(&mut lib).await, mine);

    denied(create(&mut lib, &store, mine, "app/b").await, "app/b");
    denied(create(&mut lib, &store, mine, "library").await, "library");
}

#[tokio::test]
async fn a_merge_or_a_revert_touching_another_directory_is_refused() {
    let (store, channel, main) = setup().await;
    let mut lib = client(&channel, "sp:lib");
    let author = |message: &str| (message.to_owned(), "Lib".to_owned(), "lib@x".to_owned());

    // Someone else's side change to `app/`, merged into a change of mine.
    let side = put(&store, main, &["app/b"], "side").await;
    let mine = create(&mut lib, &store, main, "lib/a").await.unwrap();
    let (message, author_name, author_email) = author("merge");
    let merge = lib
        .merge(MergeRequest {
            repo: REPO.into(),
            ours: mine.as_bytes().to_vec(),
            theirs: side.as_bytes().to_vec(),
            message,
            author_name,
            author_email,
            ..MergeRequest::default()
        })
        .await;
    denied(merge, "app/b");

    // Undoing someone else's change to `app/` on `main`.
    let (message, author_name, author_email) = author("revert");
    let revert = lib
        .revert(RevertRequest {
            repo: REPO.into(),
            head: main.as_bytes().to_vec(),
            changeset: main.as_bytes().to_vec(),
            message,
            author_name,
            author_email,
            ..RevertRequest::default()
        })
        .await;
    denied(revert, "app/b");

    // Replaying a stack that changes `app/`.
    let stacked = put(&store, side, &["lib/a"], "stacked").await;
    let rebase = lib
        .rebase(RebaseRequest {
            repo: REPO.into(),
            head: stacked.as_bytes().to_vec(),
            onto: mine.as_bytes().to_vec(),
        })
        .await;
    denied(rebase, "app/b");
}

#[tokio::test]
async fn the_land_worker_holds_a_request_to_its_submitters_grants() {
    let (store, channel, main) = setup().await;
    let mut lib = client(&channel, "sp:lib");

    // Created by someone who may change `app/`, submitted by `sp:lib`, as a
    // fast-forward and as a stack the worker rebases.
    let theirs = put(&store, main, &["app/b", "lib/a"], "theirs").await;
    let cr = submit(&mut lib, theirs).await;
    assert_eq!(cr.status(), ChangeRequestStatus::Rejected);
    assert_eq!(cr.conflicts, ["app/b"]);

    let moved = create(&mut lib, &store, main, "lib/c").await.unwrap();
    assert_eq!(
        submit(&mut lib, moved).await.status(),
        ChangeRequestStatus::Landed
    );
    let cr = submit(&mut lib, theirs).await;
    assert_eq!(cr.status(), ChangeRequestStatus::Rejected);
    assert_eq!(cr.conflicts, ["app/b"]);
    assert_eq!(main_at(&mut lib).await, moved, "main is untouched");

    // A grant revoked after the submit counts when the worker lands it.
    store.delete_grant(REPO, "sp:lib", "lib").await.unwrap();
    let mine = put(&store, moved, &["lib/a"], "mine").await;
    let id = store
        .submit_change_request(REPO, mine, moved, "sp:lib")
        .await
        .unwrap()
        .id;
    // The next submit wakes the worker, which takes the requests in order.
    submit(&mut common::control(channel.clone()), moved).await;
    let cr = store.get_change_request(REPO, id).await.unwrap().unwrap();
    assert_eq!(cr.conflicts, ["lib/a"]);
}

#[tokio::test]
async fn a_reader_reads_paths_under_its_directory_only() {
    let (_store, channel, main) = setup().await;
    let mut look = client(&channel, "sp:look");
    let head = main.as_bytes().to_vec();

    let path_log = |path: &str| PathLogRequest {
        repo: REPO.into(),
        head: head.clone(),
        path: path.into(),
        limit: 10,
    };
    look.path_log(path_log("lib/a")).await.unwrap();
    look.path_log(path_log("lib")).await.unwrap();
    denied(look.path_log(path_log("app/b")).await, "app/b");

    let blame = |path: &str| BlameRequest {
        repo: REPO.into(),
        changeset: head.clone(),
        path: path.into(),
    };
    look.blame(blame("lib/a")).await.unwrap();
    denied(look.blame(blame("app/b")).await, "app/b");

    let diff = look
        .diff(DiffRequest {
            repo: REPO.into(),
            old: None,
            new: head.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    let paths: Vec<_> = diff.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["lib/a"], "only what it may read");
}

#[tokio::test]
async fn an_admin_of_a_directory_cannot_move_a_ref() {
    let (store, channel, main) = setup().await;
    let grant = Grant {
        principal: "sp:half".to_owned(),
        path_prefix: "lib".to_owned(),
        role: Role::Admin,
    };
    store.put_grant(REPO, &grant).await.unwrap();
    let next = put(&store, main, &["lib/a"], "next").await;
    let err = client(&channel, "sp:half")
        .move_ref(MoveRefRequest {
            repo: REPO.into(),
            name: "main".into(),
            target: next.as_bytes().to_vec(),
            expected_version: Some(0),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied, "{err}");
    assert!(err.message().contains("whole repository"), "{err}");
}
