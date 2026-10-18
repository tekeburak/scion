//! `Merge`, the server half of `scn merge`: a release fix merged into a head
//! records its changes against the head only, blame follows the fix to the
//! changeset that wrote it, and the merge can be replayed onto a newer
//! `main` by `Rebase`.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use scion_derived::test_util::{REPO, changeset, tracked};
use scion_derived::{Blame, Deriver};
use scion_proto::v1::MergeRequest;
use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_server::{Merged, Rebased, merge, rebase};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{ChangesetId, FileChange, Identity, RepoPath, Timestamp};
use tokio::net::TcpListener;
use tonic::Code;
use tonic::transport::Channel;

fn path(p: &str) -> RepoPath {
    RepoPath::new(p).unwrap()
}

/// Upload `files` and create a changeset of them on `parent` through the
/// shared create path.
async fn create(
    store: &MemoryStore,
    parent: Option<ChangesetId>,
    files: &[(&str, &str)],
    msg: &str,
) -> ChangesetId {
    let mut changes = BTreeMap::new();
    for (p, content) in files {
        store
            .put_blob(Bytes::copy_from_slice(content.as_bytes()))
            .await
            .unwrap();
        changes.insert(path(p), tracked(content.as_bytes()));
    }
    let cs = changeset(parent.into_iter().collect(), changes, msg);
    scion_server::create_changesets(store, store, &[cs])
        .await
        .unwrap()[0]
        .0
}

fn author() -> (Identity, Timestamp) {
    (Identity::new("M", "m@example.com"), Timestamp::new(1, 0))
}

async fn blame(store: &MemoryStore, cs: ChangesetId, p: &str) -> Vec<(ChangesetId, u64)> {
    let d = Deriver::new(REPO, store, store);
    Blame::for_path(&d, cs, &path(p))
        .await
        .unwrap()
        .unwrap()
        .runs
}

/// `base` ─ `main` (edits `g`), and `base` ─ `fix` (`release/1`, edits
/// line 2 of `f`).
async fn release() -> (MemoryStore, [ChangesetId; 3]) {
    let store = MemoryStore::with_repo(REPO);
    let base = create(&store, None, &[("f", "1\n2\n3\n"), ("g", "g\n")], "base").await;
    let main = create(&store, Some(base), &[("g", "main\n")], "main").await;
    let fix = create(&store, Some(base), &[("f", "1\nfix\n3\n")], "fix").await;
    (store, [base, main, fix])
}

#[tokio::test]
async fn merges_a_release_fix_and_blames_it_to_the_fix() {
    let (store, [base, main, fix]) = release().await;
    let Merged::Clean {
        merged,
        created: true,
        ..
    } = merge(
        &store,
        &store,
        REPO,
        (main, fix),
        author(),
        "Merge release/1",
    )
    .await
    .unwrap()
    else {
        panic!("clean")
    };
    let cs = store.get_changeset(REPO, merged).await.unwrap().unwrap();
    assert_eq!(cs.parents, [main, fix]);
    assert_eq!(cs.message, "Merge release/1");
    // Against `main` only: `g` is not mentioned, `f` names its source.
    let names: Vec<&str> = cs.file_changes.keys().map(RepoPath::as_str).collect();
    assert_eq!(names, ["f"]);
    let FileChange::Tracked { copy_from, .. } = &cs.file_changes[&path("f")] else {
        panic!("tracked")
    };
    assert_eq!(copy_from, &Some((path("f"), fix)));
    assert_eq!(
        blame(&store, merged, "f").await,
        [(base, 1), (fix, 1), (base, 1)]
    );

    // Merging again changes nothing.
    assert!(matches!(
        merge(&store, &store, REPO, (merged, fix), author(), "again")
            .await
            .unwrap(),
        Merged::Clean { merged: m, created: false, .. } if m == merged
    ));

    // `main` moved on: the merge is replayed onto it, keeping `fix` as its
    // second parent and the fix's blame.
    let newer = create(&store, Some(main), &[("h", "h\n")], "newer").await;
    let Rebased::Clean { head, created, .. } =
        rebase(&store, &store, REPO, merged, newer).await.unwrap()
    else {
        panic!("clean")
    };
    assert_eq!(created, 1);
    let replayed = store.get_changeset(REPO, head).await.unwrap().unwrap();
    assert_eq!(replayed.parents, [newer, fix]);
    assert_eq!(
        blame(&store, head, "f").await,
        [(base, 1), (fix, 1), (base, 1)]
    );
}

#[tokio::test]
async fn a_conflict_names_the_paths() {
    let (store, [_, main, fix]) = release().await;
    let mine = create(&store, Some(main), &[("f", "1\nmine\n3\n")], "mine").await;
    assert_eq!(
        merge(&store, &store, REPO, (mine, fix), author(), "m")
            .await
            .unwrap(),
        Merged::Conflict {
            paths: vec![path("f")]
        }
    );
}

#[tokio::test]
async fn unknown_heads_are_not_found() {
    let (store, [_, main, _]) = release().await;
    let store = Arc::new(store);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(scion_server::serve(listener, store.clone(), store));
    let channel = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = ControlServiceClient::new(channel);
    let unknown = ChangesetId::hash(b"unknown");
    for (repo, ours, theirs) in [
        (REPO, main, unknown),
        (REPO, unknown, main),
        ("other", main, main),
    ] {
        let err = client
            .merge(MergeRequest {
                repo: repo.into(),
                ours: ours.as_bytes().to_vec(),
                theirs: theirs.as_bytes().to_vec(),
                message: "m".into(),
                ..MergeRequest::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::NotFound, "{repo} {ours} {theirs}");
    }
}
