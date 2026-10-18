//! The serial land worker over a real transport.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::join_all;
use scion_derived::test_util::{REPO, changeset, tracked};
use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::{
    ChangeRequest, ChangeRequestStatus, GetChangeRequestRequest, GetRefRequest, LogRequest,
    SubmitChangeRequestRequest,
};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{Changeset, ChangesetId, Decode, RepoPath};
use tokio::net::TcpListener;
use tonic::transport::Channel;

async fn commit(store: &MemoryStore, parent: ChangesetId, path: &str) -> ChangesetId {
    let content = path.as_bytes();
    store
        .put_blob(Bytes::copy_from_slice(content))
        .await
        .unwrap();
    let changes = BTreeMap::from([(RepoPath::new(path).unwrap(), tracked(content))]);
    store
        .put_changeset(&changeset(vec![parent], changes, path))
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

async fn start(store: Arc<MemoryStore>) -> ControlServiceClient<Channel> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(scion_server::serve(listener, store.clone(), store));
    let channel = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    ControlServiceClient::new(channel)
}

/// Read request `id` until the worker has finished it.
async fn finished(c: &mut ControlServiceClient<Channel>, id: u64) -> ChangeRequest {
    for _ in 0..1000 {
        let cr = c
            .get_change_request(GetChangeRequestRequest {
                repo: REPO.into(),
                id,
            })
            .await
            .unwrap()
            .into_inner();
        if !matches!(
            cr.status(),
            ChangeRequestStatus::Pending | ChangeRequestStatus::Landing
        ) {
            return cr;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("request {id} did not finish");
}

/// `main`'s first-parent history, newest first.
async fn main_log(c: &mut ControlServiceClient<Channel>) -> Vec<Changeset> {
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
    let waiting = store.submit_change_request(REPO, head, root).await.unwrap();
    let mut c = start(store.clone()).await;

    let cr = finished(&mut c, waiting.id).await;
    assert_eq!(cr.status(), ChangeRequestStatus::Landed);
    assert_eq!(cr.landed.as_deref(), Some(head.as_bytes().as_slice()));

    // `main` already contains `head`: landing it again lands nothing.
    let again = c
        .submit_change_request(SubmitChangeRequestRequest {
            repo: REPO.into(),
            head: head.as_bytes().to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    let cr = finished(&mut c, again.id).await;
    assert_eq!(cr.status(), ChangeRequestStatus::Rejected);
    assert_eq!(main_log(&mut c).await.len(), 2);
}
