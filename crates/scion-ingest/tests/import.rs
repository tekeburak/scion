//! Import into a store, serve it, and check out every first-parent commit:
//! each checkout must equal the git tree, byte for byte and mode for mode.

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use common::{commit, fixture, git, git_with, write};
use scion_client::{Remote, SparseProfile, checkout};
use scion_derived::{Deriver, RootTree};
use scion_ingest::{GIT_SHA, GitRepo, IngestError, MAIN_REF, ObjectId, import};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{BlobId, ChangesetId};
use tokio::net::TcpListener;

/// The key the test server signs tokens with.
fn key() -> scion_server::TokenKey {
    scion_server::TokenKey::from_hex(&"5c".repeat(32)).unwrap()
}

/// A token of [`key`], valid for an hour.
fn token() -> String {
    let hour = std::time::Duration::from_secs(3600);
    scion_server::token(&key(), "sp:test", hour, std::time::SystemTime::now()).unwrap()
}

/// Make `sp:test`, the principal of [`token`], an admin of the server and of
/// `repo` if it exists.
async fn admit(meta: &dyn scion_store::MetadataStore, repo: &str) {
    let admin = scion_store::Grant {
        principal: "sp:test".into(),
        path_prefix: String::new(),
        role: scion_store::Role::Admin,
    };
    meta.put_grant(scion_store::SERVER_GRANTS, &admin)
        .await
        .unwrap();
    let _ = meta.put_grant(repo, &admin).await;
}

const REPO: &str = "mono";

/// path → (git mode, contents) of commit `rev`'s tree, from `git ls-tree`.
fn git_tree(dir: &Path, rev: &str) -> BTreeMap<String, (String, Vec<u8>)> {
    let listing = git(dir, &["ls-tree", "-r", "-z", rev]);
    listing
        .split('\0')
        .filter(|l| !l.is_empty())
        .map(|line| {
            let (meta, path) = line.split_once('\t').unwrap();
            let m: Vec<&str> = meta.split(' ').collect();
            let data = git_with(dir, &["cat-file", "blob", m[2]], "", None);
            (path.to_owned(), (m[0].to_owned(), data))
        })
        .collect()
}

/// The same view of a checkout on disk, `.scn/` excluded.
fn disk_tree(root: &Path) -> BTreeMap<String, (String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (String, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_name() == ".scn" {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            if meta.is_dir() {
                walk(root, &path, out);
            } else if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.insert(
                    rel,
                    (
                        "120000".into(),
                        target.into_os_string().into_encoded_bytes(),
                    ),
                );
            } else {
                let mode = if meta.permissions().mode() & 0o111 != 0 {
                    "100755"
                } else {
                    "100644"
                };
                out.insert(rel, (mode.into(), std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkouts_equal_git_trees() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("git");
    std::fs::create_dir(&dir).unwrap();
    fixture(&dir);

    let store = Arc::new(MemoryStore::new());
    common::init(&*store, &*store, REPO).await;
    let git_repo = GitRepo::open(&dir).unwrap();
    let tip = import(&git_repo, "main", REPO, &*store, &*store)
        .await
        .unwrap()
        .tip;
    let main = store.get_ref(REPO, MAIN_REF).await.unwrap().unwrap();
    assert_eq!(main.target, tip);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    admit(&*store, REPO).await;
    tokio::spawn(scion_server::serve(
        listener,
        store.clone(),
        store.clone(),
        key(),
    ));

    // `main` clones to the git tree.
    let remote = Remote::with_token(&server, &token()).await.unwrap();
    let wc = tmp.path().join("wc");
    scion_client::clone(remote.clone(), &server, REPO, &wc, SparseProfile::full())
        .await
        .unwrap();
    assert_eq!(disk_tree(&wc), git_tree(&dir, "main"));

    // So does every first-parent commit, checked out by id.
    let deriver = Deriver::new(REPO, &*store, &*store);
    let mut next: Option<ChangesetId> = Some(tip);
    let mut seen = 0;
    while let Some(id) = next {
        let cs = store.get_changeset(REPO, id).await.unwrap().unwrap();
        let sha = ObjectId::try_from(cs.extra[GIT_SHA].as_slice()).unwrap();
        let root = deriver.derive::<RootTree>(id).await.unwrap().0;
        let out = tmp.path().join(format!("at-{seen}"));
        checkout(&remote, REPO, &out, root, &SparseProfile::full())
            .await
            .unwrap();
        assert_eq!(
            disk_tree(&out),
            git_tree(&dir, &sha.to_string()),
            "at {sha}"
        );
        next = cs.first_parent();
        seen += 1;
    }
    assert_eq!(
        seen,
        git(&dir, &["rev-list", "--first-parent", "main"])
            .lines()
            .count()
    );
}

#[tokio::test]
async fn reimport_is_a_no_op_and_new_commits_fast_forward() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fixture(dir);
    let store = MemoryStore::new();
    common::init(&store, &store, REPO).await;
    let first = import(&GitRepo::open(dir).unwrap(), "main", REPO, &store, &store)
        .await
        .unwrap();
    let before = store.get_ref(REPO, MAIN_REF).await.unwrap().unwrap();

    let again = import(&GitRepo::open(dir).unwrap(), "main", REPO, &store, &store)
        .await
        .unwrap();
    assert_eq!(first.created, 5);
    assert_eq!((again.tip, again.created), (first.tip, 0));
    assert_eq!(
        store.get_ref(REPO, MAIN_REF).await.unwrap().unwrap(),
        before
    );

    write(dir, "later.txt", "later\n");
    commit(dir, "later");
    let later = import(&GitRepo::open(dir).unwrap(), "main", REPO, &store, &store)
        .await
        .unwrap();
    let after = store.get_ref(REPO, MAIN_REF).await.unwrap().unwrap();
    assert_eq!((after.target, later.created), (later.tip, 1));
    assert_eq!(after.version, before.version + 1);
    let cs = store.get_changeset(REPO, later.tip).await.unwrap().unwrap();
    assert_eq!(cs.parents, vec![first.tip]);
}

#[tokio::test]
async fn an_unknown_repository_is_refused_before_anything_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path());
    let store = MemoryStore::new();
    common::init(&store, &store, REPO).await;
    let git = GitRepo::open(tmp.path()).unwrap();

    let err = import(&git, "main", "mnoo", &store, &store)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, IngestError::NoRepository(r) if r == "mnoo"),
        "{err}"
    );
    assert!(err.to_string().contains("scn init"), "{err}");
    let readme = BlobId::hash(b"one\n");
    assert_eq!(store.has_blobs(&[readme]).await.unwrap(), [false]);
    assert_eq!(store.ingest_checkpoints("mnoo").await.unwrap(), vec![]);
}
