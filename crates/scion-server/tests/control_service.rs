//! Control-plane read RPCs over a real transport, backed by an in-memory
//! metadata and object store.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use scion_derived::test_util::{CountingMeta, REPO, changeset, tracked};
use scion_derived::{Blame, Deriver, PathHistory};
use scion_proto::v1::{
    BlameRequest, BlameRun, DiffRequest, GetRefRequest, LogRequest, MergeBaseRequest,
    PathLogRequest,
};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{BlobId, Changeset, ChangesetId, Decode, RepoPath};
use tonic::Code;

/// `root` ─ `left` (main), and `root` ─ `right` on the side.
struct Repo {
    client: common::Control,
    root: ChangesetId,
    left: ChangesetId,
    right: ChangesetId,
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

fn bytes(id: ChangesetId) -> Vec<u8> {
    id.as_bytes().to_vec()
}

async fn commit(
    store: &MemoryStore,
    parents: Vec<ChangesetId>,
    files: &[(&str, &[u8])],
    msg: &str,
) -> ChangesetId {
    let mut changes = BTreeMap::new();
    for (p, content) in files {
        store
            .put_blob(Bytes::copy_from_slice(content))
            .await
            .unwrap();
        changes.insert(path(p), tracked(content));
    }
    store
        .put_changeset(&changeset(parents, changes, msg))
        .await
        .unwrap()
}

async fn start() -> Repo {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = commit(&store, vec![], &[("a.txt", b"1\n2\n")], "root").await;
    let left = commit(
        &store,
        vec![root],
        &[("a.txt", b"1\nX\n2\n"), ("dir/b.txt", b"b\n")],
        "left",
    )
    .await;
    let right = commit(&store, vec![root], &[("c.txt", b"c\n")], "right").await;
    store.create_ref(REPO, "main", left).await.unwrap();

    let channel = common::serve(store.clone(), store).await;
    Repo {
        client: common::control(channel),
        root,
        left,
        right,
    }
}

fn decode_log(changesets: &[Vec<u8>]) -> Vec<ChangesetId> {
    changesets
        .iter()
        .map(|b| Changeset::decode(b).unwrap().id())
        .collect()
}

#[tokio::test]
async fn get_ref_returns_target_version_and_root_tree() {
    let mut r = start().await;
    let resp = r
        .client
        .get_ref(GetRefRequest {
            repo: REPO.into(),
            name: "main".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.target, bytes(r.left));
    assert_eq!(resp.version, 0);
    assert_eq!(resp.root_tree.len(), 32);

    let err = r
        .client
        .get_ref(GetRefRequest {
            repo: REPO.into(),
            name: "release/1".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn log_and_path_log_follow_first_parent() {
    let mut r = start().await;
    let log = r
        .client
        .log(LogRequest {
            repo: REPO.into(),
            head: bytes(r.left),
            limit: 10,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(decode_log(&log.changesets), vec![r.left, r.root]);

    let limited = r
        .client
        .log(LogRequest {
            repo: REPO.into(),
            head: bytes(r.left),
            limit: 1,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(decode_log(&limited.changesets), vec![r.left]);

    let dir = r
        .client
        .path_log(PathLogRequest {
            repo: REPO.into(),
            head: bytes(r.left),
            path: "dir".into(),
            limit: 10,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(decode_log(&dir.changesets), vec![r.left]);

    let a = r
        .client
        .path_log(PathLogRequest {
            repo: REPO.into(),
            head: bytes(r.left),
            path: "a.txt".into(),
            limit: 10,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(decode_log(&a.changesets), vec![r.left, r.root]);
}

#[tokio::test]
async fn diff_reports_files_in_path_order() {
    let mut r = start().await;
    let diff = r
        .client
        .diff(DiffRequest {
            repo: REPO.into(),
            old: Some(bytes(r.root)),
            new: bytes(r.left),
        })
        .await
        .unwrap()
        .into_inner();
    let summary: Vec<(&str, bool, bool)> = diff
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.old.is_some(), e.new.is_some()))
        .collect();
    assert_eq!(
        summary,
        vec![("a.txt", true, true), ("dir/b.txt", false, true)]
    );
    let new = diff.entries[1].new.as_ref().unwrap();
    assert_eq!(new.id, BlobId::hash(b"b\n").as_bytes());
    assert_eq!(new.size, 2);

    let from_empty = r
        .client
        .diff(DiffRequest {
            repo: REPO.into(),
            old: None,
            new: bytes(r.root),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(from_empty.entries.len(), 1);
    assert!(from_empty.entries[0].old.is_none());
}

#[tokio::test]
async fn merge_base_of_siblings_is_their_parent() {
    let mut r = start().await;
    let base = r
        .client
        .merge_base(MergeBaseRequest {
            repo: REPO.into(),
            a: bytes(r.left),
            b: bytes(r.right),
        })
        .await
        .unwrap()
        .into_inner()
        .base;
    assert_eq!(base, Some(bytes(r.root)));
}

#[tokio::test]
async fn blame_attributes_each_line() {
    let mut r = start().await;
    let runs = r
        .client
        .blame(BlameRequest {
            repo: REPO.into(),
            changeset: bytes(r.left),
            path: "a.txt".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .runs;
    let run = |cs, lines| BlameRun {
        changeset: bytes(cs),
        lines,
    };
    assert_eq!(runs, vec![run(r.root, 1), run(r.left, 1), run(r.root, 1)]);

    for missing in ["dir", "nope.txt"] {
        let err = r
            .client
            .blame(BlameRequest {
                repo: REPO.into(),
                changeset: bytes(r.left),
                path: missing.into(),
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::NotFound, "{missing}");
    }
}

/// Phase 10.7: a cold `Blame` replies from the file's own versions; the
/// node of the changeset that last changed it, which holds every file that
/// changeset changed, is stored after the reply and holds the same runs.
#[tokio::test]
async fn cold_blame_replies_before_its_node_is_stored() {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = commit(&store, vec![], &[("a.txt", b"1\n"), ("b.txt", b"0\n")], "0").await;
    let mut head = root;
    let mut busy = String::from("0\n");
    for i in 1..40 {
        busy += &(i.to_string() + "\n");
        head = commit(&store, vec![head], &[("b.txt", busy.as_bytes())], "busy").await;
    }
    let both = commit(
        &store,
        vec![head],
        &[("a.txt", b"1\n2\n"), ("b.txt", b"b\n")],
        "both",
    )
    .await;
    let shared: &'static MemoryStore = Box::leak(Box::new(store.clone()));
    let d = Deriver::new(REPO, shared, shared);
    d.derive::<PathHistory>(both).await.unwrap();

    let mut client = common::control(common::serve(store.clone(), store.clone()).await);
    let blame = BlameRequest {
        repo: REPO.into(),
        changeset: bytes(both),
        path: "a.txt".into(),
    };
    let runs = client.blame(blame.clone()).await.unwrap().into_inner().runs;
    let run = |cs, lines| BlameRun {
        changeset: bytes(cs),
        lines,
    };
    assert_eq!(runs, vec![run(root, 1), run(both, 1)]);

    let mut stored = None;
    for _ in 0..500 {
        stored = d.fetch::<Blame>(both).await.unwrap();
        if stored.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(stored.is_some(), "the node is stored after the reply");
    assert_eq!(
        Blame::stored_file(&d, both, &path("a.txt"))
            .await
            .unwrap()
            .unwrap()
            .runs,
        vec![(root, 1), (both, 1)]
    );
    assert_eq!(client.blame(blame).await.unwrap().into_inner().runs, runs);
}

#[tokio::test]
async fn bad_input_and_unknown_changesets_are_rejected() {
    let mut r = start().await;
    let err = r
        .client
        .log(LogRequest {
            repo: REPO.into(),
            head: vec![1, 2, 3],
            limit: 1,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);

    let err = r
        .client
        .path_log(PathLogRequest {
            repo: REPO.into(),
            head: bytes(r.left),
            path: "../etc".into(),
            limit: 1,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);

    let unknown = ChangesetId::hash(b"unknown");
    let err = r
        .client
        .diff(DiffRequest {
            repo: REPO.into(),
            old: None,
            new: bytes(unknown),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);

    // Changesets are scoped by repository.
    let err = r
        .client
        .log(LogRequest {
            repo: "other".into(),
            head: bytes(r.left),
            limit: 1,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

/// Phase 10.2: a 428-result path log on a cold server costs the grants
/// query, the `PathHistory` pointer at the head and one `get_changesets`,
/// besides its history-page reads (Phase 10.3 moved those from the object
/// store to the metadata store): one page per change and one per anchor,
/// read in one query per step of the walk (Phase 10.7).
#[tokio::test]
async fn path_log_of_428_changesets_makes_three_metadata_queries() {
    let shared = Arc::new(MemoryStore::with_repo(REPO));
    let store: &'static MemoryStore = Box::leak(Box::new(shared.clone()));
    let mut head = commit(store, vec![], &[("f.txt", b"0\n")], "0").await;
    for i in 1..428 {
        let content = format!("{i}\n");
        head = commit(store, vec![head], &[("f.txt", content.as_bytes())], "edit").await;
    }
    Deriver::new(REPO, store, store)
        .derive::<PathHistory>(head)
        .await
        .unwrap();

    let counting = Arc::new(CountingMeta::new(store));
    let mut client = common::control(common::serve(counting.clone(), shared).await);
    counting.queries.store(0, Ordering::Relaxed);
    let log = client
        .path_log(PathLogRequest {
            repo: REPO.into(),
            head: bytes(head),
            path: "f.txt".into(),
            limit: 1000,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(log.changesets.len(), 428);
    let pages = counting.page_gets();
    assert!(pages <= 2 + 428 + 428 / 16, "{pages} page reads");
    // Phase 10.7: the chain is walked one step at a time, each step one
    // read of every page it needs: the lookup, then about 428 / 16 anchors
    // and the 16 entries below the last.
    let steps = 1 + 428 / 16 + 16 + 1;
    assert!(
        counting.queries() <= 3 + steps,
        "{} queries, at most {} besides {steps} page reads",
        counting.queries(),
        3
    );
}
