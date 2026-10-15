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
use scion_ingest::{GIT_SHA, GitRepo, MAIN_REF, ObjectId, import};
use scion_store::{MemoryStore, MetadataStore};
use scion_types::ChangesetId;
use tokio::net::TcpListener;

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
    let git_repo = GitRepo::open(&dir).unwrap();
    let tip = import(&git_repo, "main", REPO, &*store, &*store)
        .await
        .unwrap();
    let main = store.get_ref(REPO, MAIN_REF).await.unwrap().unwrap();
    assert_eq!(main.target, tip);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(scion_server::serve(listener, store.clone(), store.clone()));

    // `main` clones to the git tree.
    let wc = tmp.path().join("wc");
    scion_client::clone(&server, REPO, &wc, SparseProfile::full())
        .await
        .unwrap();
    assert_eq!(disk_tree(&wc), git_tree(&dir, "main"));

    // So does every first-parent commit, checked out by id.
    let remote = Remote::connect(&server).await.unwrap();
    let deriver = Deriver::new(REPO, &*store, &*store);
    let mut next: Option<ChangesetId> = Some(tip);
    let mut seen = 0;
    while let Some(id) = next {
        let cs = store.get_changeset(REPO, id).await.unwrap().unwrap();
        let sha = ObjectId::try_from(cs.extra[GIT_SHA].as_slice()).unwrap();
        let root = deriver.derive::<RootTree>(id).await.unwrap().0;
        let out = tmp.path().join(format!("at-{seen}"));
        checkout(&remote, &out, root, &SparseProfile::full())
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
    let first = import(&GitRepo::open(dir).unwrap(), "main", REPO, &store, &store)
        .await
        .unwrap();
    let before = store.get_ref(REPO, MAIN_REF).await.unwrap().unwrap();

    let again = import(&GitRepo::open(dir).unwrap(), "main", REPO, &store, &store)
        .await
        .unwrap();
    assert_eq!(again, first);
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
    assert_eq!(after.target, later);
    assert_eq!(after.version, before.version + 1);
    let cs = store.get_changeset(REPO, later).await.unwrap().unwrap();
    assert_eq!(cs.parents, vec![first]);
}
