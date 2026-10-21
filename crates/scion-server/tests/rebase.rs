//! `Rebase`, the server half of `scn update`: a stack replayed onto another
//! head ends at the same tree as merging the two, and conflicts and
//! already-contained heads are handled without creating anything wrong.
//! Replaying a merge is covered with `Merge` in `merge.rs`.

use std::collections::BTreeMap;

use bytes::Bytes;
use proptest::prelude::*;
use scion_derived::test_util::{Flat, REPO, changeset, materialize, tracked};
use scion_derived::{Deriver, RootTree};
use scion_server::{Rebased, Scope, rebase};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{ChangesetId, FileChange, RepoPath};

fn path(p: &str) -> RepoPath {
    RepoPath::new(p).unwrap()
}

/// Upload every blob `changes` names, then create a changeset of them on
/// `parent` through the shared create path.
async fn create(
    store: &MemoryStore,
    parent: Option<ChangesetId>,
    changes: BTreeMap<RepoPath, Vec<u8>>,
    deleted: &[RepoPath],
    msg: &str,
) -> ChangesetId {
    let mut file_changes = BTreeMap::new();
    for (p, content) in changes {
        file_changes.insert(p, tracked(&content));
        store.put_blob(Bytes::from(content)).await.unwrap();
    }
    for p in deleted {
        file_changes.insert(p.clone(), FileChange::Deleted);
    }
    let cs = changeset(parent.into_iter().collect(), file_changes, msg);
    scion_server::create_changesets(store, store, &[cs])
        .await
        .unwrap()[0]
        .0
}

async fn tree(store: &MemoryStore, cs: ChangesetId) -> Flat {
    let d = Deriver::new(REPO, store, store);
    materialize(store, d.derive::<RootTree>(cs).await.unwrap().0).await
}

fn files(entries: &[(&str, &str)]) -> BTreeMap<RepoPath, Vec<u8>> {
    entries
        .iter()
        .map(|(p, c)| (path(p), c.as_bytes().to_vec()))
        .collect()
}

#[tokio::test]
async fn replays_a_stack_and_reports_conflicts() {
    let store = MemoryStore::with_repo(REPO);
    let base = create(
        &store,
        None,
        files(&[("a", "1\n2\n3\n"), ("b", "b\n")]),
        &[],
        "base",
    )
    .await;
    let main = create(&store, Some(base), files(&[("b", "main\n")]), &[], "main").await;
    let one = create(&store, Some(base), files(&[("c", "c\n")]), &[], "one").await;
    // Already on main: replaying it changes nothing, so it is dropped.
    let same = create(&store, Some(one), files(&[("b", "main\n")]), &[], "same").await;
    let two = create(
        &store,
        Some(same),
        files(&[("a", "1\n2\n3\n4\n")]),
        &[],
        "two",
    )
    .await;

    let Rebased::Clean { head, created, .. } =
        rebase(&store, &store, REPO, two, main, &Scope::whole())
            .await
            .unwrap()
    else {
        panic!("clean")
    };
    assert_eq!(created, 2);
    let replayed = store.get_changeset(REPO, head).await.unwrap().unwrap();
    assert_eq!(replayed.message, "two");
    let first = replayed.parents[0];
    let first_cs = store.get_changeset(REPO, first).await.unwrap().unwrap();
    assert_eq!(
        (first_cs.message.as_str(), first_cs.parents[0]),
        ("one", main)
    );
    let flat = tree(&store, head).await;
    let names: Vec<&str> = flat.keys().map(RepoPath::as_str).collect();
    assert_eq!(names, ["a", "b", "c"]);

    // Again from the result: it already contains main.
    assert_eq!(
        rebase(&store, &store, REPO, head, main, &Scope::whole())
            .await
            .unwrap(),
        Rebased::Clean {
            head,
            root: Deriver::new(REPO, &store, &store)
                .derive::<RootTree>(head)
                .await
                .unwrap()
                .0,
            created: 0
        }
    );
    // A head main already contains moves to main.
    let Rebased::Clean { head: ff, .. } = rebase(&store, &store, REPO, base, main, &Scope::whole())
        .await
        .unwrap()
    else {
        panic!("clean")
    };
    assert_eq!(ff, main);

    // Both sides rewrite line 2 of `a`.
    let theirs = create(&store, Some(base), files(&[("a", "1\nX\n3\n")]), &[], "x").await;
    let mine = create(&store, Some(base), files(&[("a", "1\nY\n3\n")]), &[], "y").await;
    assert_eq!(
        rebase(&store, &store, REPO, mine, theirs, &Scope::whole())
            .await
            .unwrap(),
        Rebased::Conflict {
            changeset: mine,
            paths: vec![path("a")]
        }
    );
}

/// A head that reaches `onto` only through a merge's second parent is
/// replayed, not taken as it is: landing it would take `main`'s history off
/// `main`'s first-parent line.
#[tokio::test]
async fn a_merge_of_main_into_a_stack_is_replayed_onto_main() {
    let store = MemoryStore::with_repo(REPO);
    let base = create(&store, None, files(&[("a", "a\n")]), &[], "base").await;
    let main = create(&store, Some(base), files(&[("b", "b\n")]), &[], "main").await;
    let mine = create(&store, Some(base), files(&[("c", "c\n")]), &[], "mine").await;
    let merge = changeset(
        vec![mine, main],
        files(&[("b", "b\n")])
            .into_iter()
            .map(|(p, c)| (p, tracked(&c)))
            .collect(),
        "merge main",
    );
    let merge = scion_server::create_changesets(&store, &store, &[merge])
        .await
        .unwrap()[0]
        .0;

    let Rebased::Clean { head, created, .. } =
        rebase(&store, &store, REPO, merge, main, &Scope::whole())
            .await
            .unwrap()
    else {
        panic!("clean")
    };
    assert_ne!(head, merge);
    assert_eq!(created, 1, "the merge itself replays as nothing");
    let replayed = store.get_changeset(REPO, head).await.unwrap().unwrap();
    assert_eq!(
        (replayed.message.as_str(), replayed.parents.as_slice()),
        ("mine", &[main][..])
    );
    let names: Vec<String> = tree(&store, head)
        .await
        .keys()
        .map(|p| p.as_str().to_owned())
        .collect();
    assert_eq!(names, ["a", "b", "c"]);
}

/// A stack of edits to paths in `pool`: each step sets or deletes some.
fn arb_stack(
    pool: &'static [&'static str],
) -> impl Strategy<Value = Vec<Vec<(usize, Option<u8>)>>> {
    prop::collection::vec(
        prop::collection::vec(
            (0..pool.len(), prop::option::weighted(0.8, any::<u8>())),
            1..4,
        ),
        1..4,
    )
}

const OURS: &[&str] = &["o1", "o2", "d/o3", "d/o4"];
const THEIRS: &[&str] = &["t1", "t2", "d/t3", "e/t4"];

async fn build_stack(
    store: &MemoryStore,
    mut parent: ChangesetId,
    pool: &[&str],
    stack: &[Vec<(usize, Option<u8>)>],
) -> ChangesetId {
    for (i, step) in stack.iter().enumerate() {
        let mut set = BTreeMap::new();
        let mut deleted = Vec::new();
        for (p, content) in step {
            let p = path(pool[*p]);
            match content {
                Some(b) => {
                    set.insert(p, vec![*b, b'\n']);
                }
                None => deleted.push(p),
            }
        }
        // A path both set and deleted in one step is set.
        deleted.retain(|p| !set.contains_key(p));
        parent = create(store, Some(parent), set, &deleted, &format!("step {i}")).await;
    }
    parent
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    /// For edits to disjoint paths, `update` (rebase theirs onto ours) ends
    /// at the tree `scion_core::merge` of the two heads describes.
    #[test]
    fn update_equals_merge(ours in arb_stack(OURS), theirs in arb_stack(THEIRS)) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let store = MemoryStore::with_repo(REPO);
            let mut all: Vec<(&str, &str)> = OURS.iter().map(|p| (*p, "base\n")).collect();
            all.extend(THEIRS.iter().map(|p| (*p, "base\n")));
            let base = create(&store, None, files(&all), &[], "base").await;
            let ours = build_stack(&store, base, OURS, &ours).await;
            let theirs = build_stack(&store, base, THEIRS, &theirs).await;

            let Rebased::Clean { head, .. } = rebase(&store, &store, REPO, theirs, ours, &Scope::whole()).await.unwrap() else {
                panic!("disjoint edits cannot conflict")
            };
            let d = Deriver::new(REPO, &store, &store);
            let merged = scion_core::merge(&d, ours, theirs).await.unwrap();
            prop_assert!(merged.is_clean());
            let mut expected = tree(&store, ours).await;
            for (p, change) in merged.changes {
                match change {
                    FileChange::Deleted => {
                        expected.remove(&p);
                    }
                    FileChange::Tracked { content, kind, size, .. } => {
                        expected.insert(p, (content, kind, size));
                    }
                }
            }
            prop_assert_eq!(tree(&store, head).await, expected);
            Ok(())
        })?;
    }
}
