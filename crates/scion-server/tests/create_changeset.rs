//! `CreateChangeset` over a real transport: upload blobs through
//! `ObjectService`, then create changesets through `ControlService`.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use scion_derived::test_util::{REPO, changeset, tracked};
use scion_derived::{Blame, Deriver, PathHistory, RootTree, SkipList};
use scion_proto::v1::{CreateChangesetRequest, InitRepoRequest, LogRequest, PutBlobRequest};
use scion_store::{MemoryStore, MetadataStore, Role};
use scion_types::{Changeset, ChangesetId, Decode, Encode, FileChange, RepoPath, TreeId};
use tonic::Code;

struct Server {
    store: Arc<MemoryStore>,
    control: common::Control,
    objects: common::Objects,
}

async fn start() -> Server {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let channel = common::serve(store.clone(), store.clone()).await;
    Server {
        store,
        control: common::control(channel.clone()),
        objects: common::objects(channel),
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
                repo: REPO.into(),
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
    let tree = s.store.get_tree(REPO, child_tree).await.unwrap().unwrap();
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
    assert_eq!(s.create(&nameless).await, Err(Code::PermissionDenied));

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
    assert!(
        s.store
            .get_tree(REPO, tree)
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn init_repo_creates_a_repository_with_its_caller_as_admin() {
    let mut s = start().await;
    s.upload(b"one\n").await;
    let mut typo = changeset(vec![], files(&[("a.txt", b"one\n")]), "root");
    typo.repo = "tt".into();
    // No grant exists in a repository that does not exist.
    assert_eq!(s.create(&typo).await, Err(Code::PermissionDenied));
    let move_typo = |expected_version| scion_proto::v1::MoveRefRequest {
        repo: "tt".into(),
        name: "main".into(),
        target: typo.id().as_bytes().to_vec(),
        expected_version,
    };
    for expected in [None, Some(0)] {
        let err = s.control.move_ref(move_typo(expected)).await.unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied, "{expected:?}: {err}");
    }
    assert!(
        s.store
            .get_changeset("tt", typo.id())
            .await
            .unwrap()
            .is_none()
    );

    let init = || InitRepoRequest { repo: "tt".into() };
    let resp = s.control.init_repo(init()).await.unwrap().into_inner();
    let root = Changeset::repository_root("tt");
    assert_eq!(resp.main, root.id().as_bytes());
    let main = s.store.get_ref("tt", "main").await.unwrap().unwrap();
    assert_eq!((main.target, main.version), (root.id(), resp.version));
    assert_eq!(
        s.store.grants_for("tt", "sp:test").await.unwrap(),
        [common::grant("sp:test", Role::Admin)]
    );
    let again = s.control.init_repo(init()).await.unwrap_err();
    assert_eq!(again.code(), Code::AlreadyExists, "{again}");
    assert!(s.create(&typo).await.is_ok(), "the repository exists now");
    // A root is InitRepo's alone.
    assert_eq!(s.create(&root).await, Err(Code::InvalidArgument));
}
