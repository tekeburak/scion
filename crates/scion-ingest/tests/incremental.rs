//! Incremental sync: importing a history in stages gives the same changesets
//! as one import, and a rewritten upstream is refused without writing.

mod common;

use common::{fixture, git};
use scion_ingest::{GitRepo, IngestError, MAIN_REF, import};
use scion_store::{MemoryStore, MetadataStore};

const REPO: &str = "mono";

#[tokio::test]
async fn staged_import_equals_one_import() {
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path());
    let git_repo = GitRepo::open(tmp.path()).unwrap();

    let staged = MemoryStore::new();
    let x = import(&git_repo, "main~1", REPO, &staged, &staged)
        .await
        .unwrap();
    let y = import(&git_repo, "main", REPO, &staged, &staged)
        .await
        .unwrap();
    assert_eq!(
        (x.created, y.created),
        (3, 2),
        "the second stage imports only new commits"
    );

    let once = MemoryStore::new();
    let all = import(&git_repo, "main", REPO, &once, &once).await.unwrap();
    assert_eq!(y.tip, all.tip);
    assert_eq!(
        staged.ingest_checkpoints(REPO).await.unwrap(),
        once.ingest_checkpoints(REPO).await.unwrap(),
        "every commit maps to the same changeset"
    );
    assert_eq!(
        staged
            .get_ref(REPO, MAIN_REF)
            .await
            .unwrap()
            .unwrap()
            .target,
        all.tip
    );
}

#[tokio::test]
async fn rewritten_upstream_is_refused_and_nothing_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fixture(dir);
    let store = MemoryStore::new();
    let first = import(&GitRepo::open(dir).unwrap(), "main", REPO, &store, &store)
        .await
        .unwrap();
    let checkpoints = store.ingest_checkpoints(REPO).await.unwrap();
    let main = store.get_ref(REPO, MAIN_REF).await.unwrap().unwrap();
    let old_tip = git(dir, &["rev-parse", "main"]);

    git(dir, &["commit", "-q", "--amend", "-m", "rewritten"]);
    match import(&GitRepo::open(dir).unwrap(), "main", REPO, &store, &store).await {
        Err(IngestError::Diverged { main, rev }) => {
            assert_eq!(main, format!("git commit {}", old_tip.trim()));
            assert_eq!(rev, "main");
        }
        other => panic!("expected the rewrite to be refused, got {other:?}"),
    }
    // Going backwards is refused the same way.
    assert!(matches!(
        import(&GitRepo::open(dir).unwrap(), "main~1", REPO, &store, &store).await,
        Err(IngestError::Diverged { .. })
    ));

    assert_eq!(store.ingest_checkpoints(REPO).await.unwrap(), checkpoints);
    assert_eq!(store.get_ref(REPO, MAIN_REF).await.unwrap().unwrap(), main);
    assert_eq!(main.target, first.tip);
}
