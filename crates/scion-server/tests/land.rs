//! The serial land worker over a real transport.

mod common;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use futures_util::future::join_all;
use scion_derived::test_util::{REPO, changeset, tracked};
use scion_proto::v1::{
    ChangeRequest, ChangeRequestStatus, FindLandingRequest, GetChangeRequestRequest, GetRefRequest,
    LogRequest, SubmitChangeRequestRequest,
};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{Changeset, ChangesetId, Decode, RepoPath};

async fn commit(store: &MemoryStore, parent: ChangesetId, path: &str) -> ChangesetId {
    edit(store, parent, path, path).await
}

/// A child of `parent` that writes `content` to `path`.
async fn edit(store: &MemoryStore, parent: ChangesetId, path: &str, text: &str) -> ChangesetId {
    let content = text.as_bytes();
    store
        .put_blob(Bytes::copy_from_slice(content))
        .await
        .unwrap();
    let changes = BTreeMap::from([(RepoPath::new(path).unwrap(), tracked(content))]);
    store
        .put_changeset(&changeset(vec![parent], changes, text))
        .await
        .unwrap()
}

/// A repository whose `main` is at an empty root.
async fn repository() -> (Arc<MemoryStore>, ChangesetId) {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = store
        .put_changeset(&changeset(vec![], BTreeMap::new(), "root"))
        .await
        .unwrap();
    store.create_ref(REPO, "main", root).await.unwrap();
    (store, root)
}

/// Move `main` to `target`, as another writer would.
async fn advance(store: &MemoryStore, target: ChangesetId) {
    let main = store.get_ref(REPO, "main").await.unwrap().unwrap();
    store
        .move_ref(REPO, "main", main.version, target)
        .await
        .unwrap();
}

async fn start(store: Arc<MemoryStore>) -> common::Control {
    let channel = common::serve(store.clone(), store).await;
    common::control(channel)
}

/// Request `id` once the worker has finished it.
async fn finished(c: &mut common::Control, id: u64) -> ChangeRequest {
    let cr = c
        .get_change_request(GetChangeRequestRequest {
            repo: REPO.into(),
            id,
            wait: true,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        !matches!(
            cr.status(),
            ChangeRequestStatus::Pending | ChangeRequestStatus::Landing
        ),
        "request {id} did not finish"
    );
    cr
}

/// `main`'s first-parent history, newest first.
async fn main_log(c: &mut common::Control) -> Vec<Changeset> {
    let main = c
        .get_ref(GetRefRequest {
            repo: REPO.into(),
            name: "main".into(),
        })
        .await
        .unwrap()
        .into_inner();
    c.log(LogRequest {
        repo: REPO.into(),
        head: main.target,
        limit: 1000,
    })
    .await
    .unwrap()
    .into_inner()
    .changesets
    .iter()
    .map(|b| Changeset::decode(b).unwrap())
    .collect()
}

#[tokio::test]
async fn concurrent_requests_all_land_on_a_linear_main() {
    const N: usize = 64;
    let (store, root) = repository().await;
    let mut heads = Vec::new();
    for i in 0..N {
        heads.push(commit(&store, root, &format!("f{i}.txt")).await);
    }
    let c = start(store.clone()).await;

    let submitted = join_all(heads.iter().map(|head| {
        let mut c = c.clone();
        async move {
            c.submit_change_request(SubmitChangeRequestRequest {
                repo: REPO.into(),
                head: head.as_bytes().to_vec(),
                wait: false,
            })
            .await
            .unwrap()
            .into_inner()
        }
    }))
    .await;

    let mut done = Vec::new();
    for request in &submitted {
        done.push(finished(&mut c.clone(), request.id).await);
    }

    let log = main_log(&mut c.clone()).await;
    assert_eq!(log.len(), N + 1, "every request landed once");
    assert!(log.iter().all(|cs| cs.parents.len() <= 1), "main is linear");
    assert_eq!(log.last().unwrap().id(), root);
    let on_main: BTreeMap<ChangesetId, &Changeset> = log.iter().map(|cs| (cs.id(), cs)).collect();

    let mut landed = HashSet::new();
    for (i, cr) in done.iter().enumerate() {
        assert_eq!(cr.status(), ChangeRequestStatus::Landed, "request {i}");
        let cs = ChangesetId::try_from_slice(cr.landed.as_deref().unwrap()).unwrap();
        assert!(landed.insert(cs), "each request lands its own changeset");
        let path = RepoPath::new(format!("f{i}.txt")).unwrap();
        assert!(
            on_main[&cs].file_changes.contains_key(&path),
            "request {i}'s landed changeset carries its change"
        );
    }
}

#[tokio::test]
async fn requests_waiting_at_start_land_and_a_noop_is_rejected() {
    let (store, root) = repository().await;
    let head = commit(&store, root, "a.txt").await;
    // Submitted before the server runs, as if it had restarted.
    let waiting = store
        .submit_change_request(REPO, head, root, "sp:test")
        .await
        .unwrap();
    let mut c = start(store.clone()).await;

    let cr = finished(&mut c, waiting.id).await;
    assert_eq!(cr.status(), ChangeRequestStatus::Landed);
    assert_eq!(cr.landed.as_deref(), Some(head.as_bytes().as_slice()));

    // `main` already contains `head`: landing it again lands nothing.
    let again = c
        .submit_change_request(SubmitChangeRequestRequest {
            repo: REPO.into(),
            head: head.as_bytes().to_vec(),
            wait: false,
        })
        .await
        .unwrap()
        .into_inner();
    let cr = finished(&mut c, again.id).await;
    assert_eq!(cr.status(), ChangeRequestStatus::Rejected);
    assert_eq!(main_log(&mut c).await.len(), 2);
}

async fn submit(c: &mut common::Control, head: ChangesetId) -> ChangeRequest {
    c.submit_change_request(SubmitChangeRequestRequest {
        repo: REPO.into(),
        head: head.as_bytes().to_vec(),
        wait: false,
    })
    .await
    .unwrap()
    .into_inner()
}

#[tokio::test]
async fn a_conflict_marks_the_request_and_leaves_main() {
    let (store, root) = repository().await;
    let base = edit(&store, root, "f.txt", "one\ntwo\nthree\n").await;
    advance(&store, base).await;
    let ours = edit(&store, base, "f.txt", "one\nTWO\nthree\n").await;
    let theirs = edit(&store, base, "f.txt", "one\n2\nthree\n").await;
    let mut c = start(store.clone()).await;

    let first = submit(&mut c, ours).await;
    let second = submit(&mut c, theirs).await;
    let first = finished(&mut c, first.id).await;
    assert_eq!(first.status(), ChangeRequestStatus::Landed);
    let second = finished(&mut c, second.id).await;
    assert_eq!(second.status(), ChangeRequestStatus::NeedsRebase);
    assert_eq!(second.conflicts, ["f.txt"]);
    assert_eq!(second.landed, None);
    let log = main_log(&mut c).await;
    assert_eq!(log[0].id(), ours, "main stays at the first request");
}

/// A request claimed by a worker that stopped after creating its rebased
/// changesets, and maybe after moving `main`, lands exactly once when the
/// server starts again.
#[tokio::test]
async fn a_request_left_landing_lands_once_after_a_restart() {
    for moved_main in [false, true] {
        let (store, root) = repository().await;
        let other = commit(&store, root, "x.txt").await;
        advance(&store, other).await;
        let head = commit(&store, root, "y.txt").await;
        let cr = store
            .submit_change_request(REPO, head, other, "sp:test")
            .await
            .unwrap();

        // What the stopped worker did.
        store
            .claim_next_change_request(REPO)
            .await
            .unwrap()
            .unwrap();
        let scion_server::Rebased::Clean { head: rebased, .. } = scion_server::rebase(
            &*store,
            &*store,
            REPO,
            head,
            other,
            &scion_server::Scope::whole(),
        )
        .await
        .unwrap() else {
            panic!("no conflict");
        };
        if moved_main {
            advance(&store, rebased).await;
        }

        let mut c = start(store.clone()).await;
        let cr = finished(&mut c, cr.id).await;
        assert_eq!(cr.status(), ChangeRequestStatus::Landed, "{moved_main}");
        assert_eq!(cr.landed.as_deref(), Some(rebased.as_bytes().as_slice()));
        let log: Vec<ChangesetId> = main_log(&mut c).await.iter().map(Changeset::id).collect();
        assert_eq!(log, [rebased, other, root], "landed once ({moved_main})");
    }
}

#[tokio::test]
async fn a_waiting_submit_replies_with_the_outcome() {
    let (store, root) = repository().await;
    let head = commit(&store, root, "a.txt").await;
    let mut c = start(store.clone()).await;
    let cr = c
        .submit_change_request(SubmitChangeRequestRequest {
            repo: REPO.into(),
            head: head.as_bytes().to_vec(),
            wait: true,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cr.status(), ChangeRequestStatus::Landed);
    assert_eq!(cr.landed.as_deref(), Some(head.as_bytes().as_slice()));
}

/// A change that landed rebased is found from its own id and from the id it
/// landed as, with the time it landed.
#[tokio::test]
async fn find_landing_from_the_original_and_the_landed_id() {
    let (store, root) = repository().await;
    let a = commit(&store, root, "a.txt").await;
    let b = commit(&store, root, "b.txt").await;
    let mut c = start(store.clone()).await;

    let first = submit(&mut c, a).await;
    let first = finished(&mut c, first.id).await;
    let second = submit(&mut c, b).await;
    let second = finished(&mut c, second.id).await;
    let landed = ChangesetId::try_from_slice(second.landed.as_deref().unwrap()).unwrap();
    assert_ne!(landed, b, "b was rebased onto a");

    let mut find = async |changesets: &[ChangesetId]| {
        c.find_landing(FindLandingRequest {
            repo: REPO.into(),
            changesets: changesets.iter().map(|cs| cs.as_bytes().to_vec()).collect(),
        })
        .await
        .map(|r| r.into_inner().landings)
    };
    let landings = find(&[b, root, landed, a]).await.unwrap();
    let found: Vec<_> = landings
        .iter()
        .map(|l| (l.changeset.as_slice(), l.request.as_ref().unwrap().id))
        .collect();
    assert_eq!(
        found,
        [
            (b.as_bytes().as_slice(), second.id),
            (landed.as_bytes().as_slice(), second.id),
            (a.as_bytes().as_slice(), first.id),
        ]
    );
    for l in &landings {
        let cr = l.request.as_ref().unwrap();
        assert!(cr.landed_unix_micros.unwrap() >= cr.submitted_unix_micros);
    }

    let too_many = vec![root; 1001];
    assert_eq!(
        find(&too_many).await.unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
}
