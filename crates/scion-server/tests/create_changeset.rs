//! `CreateChangeset` over a real transport: upload blobs through
//! `ObjectService`, then create changesets through `ControlService`.

use std::collections::BTreeMap;
use std::sync::Arc;

use scion_derived::test_util::{REPO, changeset, tracked};
use scion_derived::{Blame, Deriver, PathHistory, RootTree, SkipList};
use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::object_service_client::ObjectServiceClient;
use scion_proto::v1::{CreateChangesetRequest, LogRequest, PutBlobRequest};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{Changeset, ChangesetId, Decode, Encode, FileChange, RepoPath, TreeId};
use tokio::net::TcpListener;
use tonic::Code;
use tonic::transport::Channel;

struct Server {
    store: Arc<MemoryStore>,
    control: ControlServiceClient<Channel>,
    objects: ObjectServiceClient<Channel>,
}

async fn start() -> Server {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(scion_server::serve(listener, store.clone(), store.clone()));
    let channel = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    Server {
        store,
        control: ControlServiceClient::new(channel.clone()),
        objects: ObjectServiceClient::new(channel),
    }
}

fn files(files: &[(&str, &[u8])]) -> BTreeMap<RepoPath, FileChange> {
    files
        .iter()
        .map(|(p, content)| (RepoPath::new(*p).unwrap(), tracked(content)))
        .collect()
}

impl Server {
    async fn upload(&mut self, content: &[u8]) {
        self.objects
            .put_blob(PutBlobRequest {
                data: content.to_vec(),
            })
            .await
            .unwrap();
    }

    async fn create(&mut self, cs: &Changeset) -> Result<(ChangesetId, TreeId), Code> {
        let resp = self
            .control
            .create_changeset(CreateChangesetRequest {
                changeset: cs.encode_to_vec(),
            })
            .await
            .map_err(|s| s.code())?
            .into_inner();
        Ok((
            ChangesetId::try_from_slice(&resp.id).unwrap(),
            TreeId::try_from_slice(&resp.root_tree).unwrap(),
        ))
    }
}

#[tokio::test]
async fn creates_changesets_and_derives_eagerly() {
    let mut s = start().await;
    s.upload(b"one\n").await;
    let root = changeset(vec![], files(&[("a.txt", b"one\n")]), "root");
    let (root_id, root_tree) = s.create(&root).await.unwrap();
    assert_eq!(root_id, root.id());

    s.upload(b"two\n").await;
    let child = changeset(vec![root_id], files(&[("dir/b.txt", b"two\n")]), "child");
    let (child_id, child_tree) = s.create(&child).await.unwrap();
    assert_ne!(child_tree, root_tree);
    let tree = s.store.get_tree(child_tree).await.unwrap().unwrap();
    assert_eq!(tree.entries.len(), 2);

    // Idempotent: the same changeset again is the same id.
    assert_eq!(s.create(&child).await.unwrap(), (child_id, child_tree));

    let ctx = Deriver::new(REPO, &*s.store, &*s.store);
    for id in [root_id, child_id] {
        assert!(ctx.fetch::<RootTree>(id).await.unwrap().is_some());
        assert!(ctx.fetch::<SkipList>(id).await.unwrap().is_some());
        assert!(ctx.fetch::<PathHistory>(id).await.unwrap().is_some());
        assert!(
            ctx.fetch::<Blame>(id).await.unwrap().is_none(),
            "blame is lazy"
        );
    }

    let log = s
        .control
        .log(LogRequest {
            repo: REPO.into(),
            head: child_id.as_bytes().to_vec(),
            limit: 10,
        })
        .await
        .unwrap()
        .into_inner();
    let ids: Vec<ChangesetId> = log
        .changesets
        .iter()
        .map(|b| Changeset::decode(b).unwrap().id())
        .collect();
    assert_eq!(ids, vec![child_id, root_id]);
}

#[tokio::test]
async fn rejects_missing_blobs_unknown_parents_and_garbage() {
    let mut s = start().await;

    let not_uploaded = changeset(vec![], files(&[("a.txt", b"never sent\n")]), "x");
    assert_eq!(s.create(&not_uploaded).await, Err(Code::FailedPrecondition));

    s.upload(b"ok\n").await;
    let orphan = changeset(
        vec![ChangesetId::hash(b"unknown")],
        files(&[("a.txt", b"ok\n")]),
        "orphan",
    );
    assert_eq!(s.create(&orphan).await, Err(Code::FailedPrecondition));

    let mut nameless = changeset(vec![], files(&[("a.txt", b"ok\n")]), "nameless");
    nameless.repo = String::new();
    assert_eq!(s.create(&nameless).await, Err(Code::InvalidArgument));

    let err = s
        .control
        .create_changeset(CreateChangesetRequest {
            changeset: b"not a changeset".to_vec(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);

    // Nothing was written for the rejected requests.
    for cs in [&not_uploaded, &orphan] {
        let stored = s.store.get_changeset(REPO, cs.id()).await.unwrap();
        assert!(stored.is_none());
    }
}

#[tokio::test]
async fn deletions_need_no_blob() {
    let mut s = start().await;
    s.upload(b"x\n").await;
    let (root, _) = s
        .create(&changeset(vec![], files(&[("a.txt", b"x\n")]), "root"))
        .await
        .unwrap();
    let delete = changeset(
        vec![root],
        BTreeMap::from([(RepoPath::new("a.txt").unwrap(), FileChange::Deleted)]),
        "delete",
    );
    let (_, tree) = s.create(&delete).await.unwrap();
    assert!(s.store.get_tree(tree).await.unwrap().unwrap().is_empty());
}

#[tokio::test]
async fn only_the_repository_root_creates_a_repository() {
    let mut s = start().await;
    s.upload(b"one\n").await;
    let mut typo = changeset(vec![], files(&[("a.txt", b"one\n")]), "root");
    typo.repo = "tt".into();
    assert_eq!(s.create(&typo).await, Err(Code::NotFound));
    let move_typo = |expected_version| scion_proto::v1::MoveRefRequest {
        repo: "tt".into(),
        name: "main".into(),
        target: typo.id().as_bytes().to_vec(),
        expected_version,
    };
    for expected in [None, Some(0)] {
        let err = s.control.move_ref(move_typo(expected)).await.unwrap_err();
        assert_eq!(err.code(), Code::NotFound, "{expected:?}: {err}");
    }
    assert!(
        s.store
            .get_changeset("tt", typo.id())
            .await
            .unwrap()
            .is_none()
    );

    let root = Changeset::repository_root("tt");
    let (id, tree) = s.create(&root).await.unwrap();
    assert_eq!((id, tree), (root.id(), scion_types::Tree::empty().id()));
    assert_eq!(s.create(&root).await, Ok((id, tree)), "idempotent");
    assert!(s.create(&typo).await.is_ok(), "the repository exists now");
}
