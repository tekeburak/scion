//! Renames and copies become `copy_from`, so blame follows a file across them.

mod common;

use common::{commit, git, write};
use scion_derived::{Blame, Deriver};
use scion_ingest::{GitRepo, import};
use scion_store::{MemoryStore, MetadataStore};
use scion_types::{ChangesetId, FileChange, RepoPath};

const REPO: &str = "mono";

fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

/// The changesets on `main`'s first-parent line, oldest first.
async fn line(store: &MemoryStore, tip: ChangesetId) -> Vec<ChangesetId> {
    let mut out = vec![];
    let mut next = Some(tip);
    while let Some(id) = next {
        out.push(id);
        next = store
            .get_changeset(REPO, id)
            .await
            .unwrap()
            .unwrap()
            .first_parent();
    }
    out.reverse();
    out
}

async fn copy_from(
    store: &MemoryStore,
    cs: ChangesetId,
    path: &str,
) -> Option<(RepoPath, ChangesetId)> {
    match &store
        .get_changeset(REPO, cs)
        .await
        .unwrap()
        .unwrap()
        .file_changes[&p(path)]
    {
        FileChange::Tracked { copy_from, .. } => copy_from.clone(),
        FileChange::Deleted => panic!("{path} deleted"),
    }
}

async fn blame(store: &MemoryStore, cs: ChangesetId, path: &str) -> Vec<ChangesetId> {
    let d = Deriver::new(REPO, store, store);
    Blame::for_path(&d, cs, &p(path))
        .await
        .unwrap()
        .unwrap()
        .lines()
        .collect()
}

#[tokio::test]
async fn blame_follows_renames_and_copies() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    write(dir, "a.txt", "one\ntwo\nthree\nfour\nfive\nsix\n");
    write(dir, "lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    commit(dir, "c1");

    // Rename with an edit: five of six lines survive.
    git(dir, &["mv", "a.txt", "b.txt"]);
    write(dir, "b.txt", "one\ntwo\nTHREE\nfour\nfive\nsix\n");
    write(dir, "unrelated.txt", "brand new\n");
    commit(dir, "c2: rename and edit");

    // Exact rename into a directory.
    std::fs::create_dir(dir.join("c")).unwrap();
    git(dir, &["mv", "b.txt", "c/b.txt"]);
    commit(dir, "c3: move");

    // Copy of a file the same commit modifies.
    write(dir, "copy.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    write(
        dir,
        "lib.rs",
        "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n",
    );
    commit(dir, "c4: copy");

    let store = MemoryStore::new();
    let tip = import(&GitRepo::open(dir).unwrap(), "main", REPO, &store, &store)
        .await
        .unwrap()
        .tip;
    let [c1, c2, c3, c4] = line(&store, tip).await[..] else {
        panic!("four changesets");
    };

    assert_eq!(copy_from(&store, c2, "b.txt").await, Some((p("a.txt"), c1)));
    assert_eq!(copy_from(&store, c2, "unrelated.txt").await, None);
    assert_eq!(
        copy_from(&store, c3, "c/b.txt").await,
        Some((p("b.txt"), c2))
    );
    assert_eq!(
        copy_from(&store, c4, "copy.rs").await,
        Some((p("lib.rs"), c3))
    );
    assert_eq!(
        copy_from(&store, c4, "lib.rs").await,
        None,
        "modified in place"
    );

    // Unmoved lines keep the commit that wrote them before the renames.
    assert_eq!(blame(&store, c2, "b.txt").await, [c1, c1, c2, c1, c1, c1]);
    assert_eq!(blame(&store, c4, "c/b.txt").await, [c1, c1, c2, c1, c1, c1]);
    assert_eq!(blame(&store, c4, "copy.rs").await, [c1, c1, c1]);
}
