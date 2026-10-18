//! `SubmitChangeRequest`, `GetChangeRequest` and `ListChangeRequests` over a
//! real transport.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use scion_derived::test_util::{REPO, changeset, tracked};
use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::{
    ChangeRequestStatus, GetChangeRequestRequest, ListChangeRequestsRequest,
    SubmitChangeRequestRequest,
};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{ChangesetId, RepoPath};
use tokio::net::TcpListener;
use tonic::Code;
use tonic::transport::Channel;

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

fn submit(repo: &str, head: ChangesetId) -> SubmitChangeRequestRequest {
    SubmitChangeRequestRequest {
        repo: repo.into(),
        head: head.as_bytes().to_vec(),
        wait: false,
    }
}

fn get(repo: &str, id: u64) -> GetChangeRequestRequest {
    GetChangeRequestRequest {
        repo: repo.into(),
        id,
        wait: false,
    }
}

#[tokio::test]
async fn submits_and_reads_back() {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = commit(&store, vec![], "root").await;
    store.create_ref(REPO, "main", root).await.unwrap();
    let head = commit(&store, vec![root], "edit").await;
    let mut c = start(store.clone()).await;

    let submitted = c.submit_change_request(submit(REPO, head)).await.unwrap();
    let submitted = submitted.into_inner();
    assert_eq!(submitted.head, head.as_bytes());
    assert_eq!(
        submitted.base,
        root.as_bytes(),
        "base is main at submission"
    );
    assert_eq!(submitted.status(), ChangeRequestStatus::Pending);
    assert_eq!(
        (submitted.landed.as_ref(), submitted.conflicts.len()),
        (None, 0)
    );
    // The land worker finishes it; `Get` reports what it recorded.
    let mut got = submitted.clone();
    while got.status() == ChangeRequestStatus::Pending
        || got.status() == ChangeRequestStatus::Landing
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        got = c
            .get_change_request(get(REPO, submitted.id))
            .await
            .unwrap()
            .into_inner();
    }
    assert_eq!(
        (got.id, &got.head, &got.base),
        (submitted.id, &submitted.head, &submitted.base)
    );
    assert_eq!(got.status(), ChangeRequestStatus::Landed);
    assert_eq!(got.landed.as_deref(), Some(head.as_bytes().as_slice()));
}

#[tokio::test]
async fn unknown_head_repository_or_request_is_not_found() {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = commit(&store, vec![], "root").await;
    store.create_ref(REPO, "main", root).await.unwrap();
    let mut c = start(store.clone()).await;

    let unknown = ChangesetId::from_bytes([7; 32]);
    let code = |r: Result<_, tonic::Status>| r.map(|_| ()).unwrap_err().code();
    assert_eq!(
        code(c.submit_change_request(submit(REPO, unknown)).await),
        Code::NotFound
    );
    assert_eq!(
        code(c.submit_change_request(submit("nope", root)).await),
        Code::NotFound
    );
    assert_eq!(
        code(c.get_change_request(get(REPO, 1)).await),
        Code::NotFound
    );
    let id = c
        .submit_change_request(submit(REPO, root))
        .await
        .unwrap()
        .into_inner()
        .id;
    assert_eq!(
        code(c.get_change_request(get("nope", id)).await),
        Code::NotFound
    );
    assert_eq!(
        code(
            c.submit_change_request(SubmitChangeRequestRequest {
                repo: REPO.into(),
                head: vec![1, 2, 3],
                wait: false,
            })
            .await
        ),
        Code::InvalidArgument
    );
}

fn list(repo: &str, status: ChangeRequestStatus, limit: u32) -> ListChangeRequestsRequest {
    ListChangeRequestsRequest {
        repo: repo.into(),
        status: status.into(),
        limit,
    }
}

#[tokio::test]
async fn lists_newest_first() {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let root = commit(&store, vec![], "root").await;
    store.create_ref(REPO, "main", root).await.unwrap();
    let head = commit(&store, vec![root], "edit").await;
    let mut c = start(store.clone()).await;

    let mut ids = Vec::new();
    // Lands, then has nothing left to land, then lands nothing again.
    for h in [head, head, root] {
        let mut req = submit(REPO, h);
        req.wait = true;
        ids.push(c.submit_change_request(req).await.unwrap().into_inner().id);
    }
    let listed = |r: ListChangeRequestsRequest| {
        let mut c = c.clone();
        async move {
            c.list_change_requests(r)
                .await
                .unwrap()
                .into_inner()
                .requests
        }
    };
    let all = listed(list(REPO, ChangeRequestStatus::Unspecified, 10)).await;
    assert_eq!(
        all.iter().map(|cr| cr.id).collect::<Vec<_>>(),
        [ids[2], ids[1], ids[0]]
    );
    assert!(
        all.windows(2)
            .all(|w| w[0].submitted_unix_micros >= w[1].submitted_unix_micros)
    );
    assert!(all.iter().all(|cr| cr.submitted_unix_micros > 0));
    let landed = listed(list(REPO, ChangeRequestStatus::Landed, 10)).await;
    assert_eq!(landed.len(), 1);
    assert_eq!(
        (landed[0].id, landed[0].landed.as_deref()),
        (ids[0], Some(head.as_bytes().as_slice()))
    );
    let rejected = listed(list(REPO, ChangeRequestStatus::Rejected, 1)).await;
    assert_eq!(
        rejected.iter().map(|cr| cr.id).collect::<Vec<_>>(),
        [ids[2]]
    );

    let code = |r: Result<_, tonic::Status>| r.map(|_| ()).unwrap_err().code();
    assert_eq!(
        code(
            c.list_change_requests(list("nope", ChangeRequestStatus::Unspecified, 10))
                .await
        ),
        Code::NotFound
    );
    let mut bad = list(REPO, ChangeRequestStatus::Unspecified, 10);
    bad.status = 99;
    assert_eq!(
        code(c.list_change_requests(bad).await),
        Code::InvalidArgument
    );
}
