//! Conformance suite every store implementation must pass.
//!
//! Use [`conformance_tests!`](crate::conformance_tests) to instantiate the
//! suite for a concrete store:
//!
//! ```ignore
//! scion_store::conformance_tests!(my_store_conformance, || MyStore::new());
//! ```

use std::collections::BTreeMap;

use bytes::Bytes;
use scion_types::{
    BlobId, Changeset, ChangesetId, FileChange, FileType, Identity, PathComponent, RepoPath,
    Timestamp, Tree, TreeEntry,
};

use crate::{MetadataStore, ObjectStore, StoreError};

const REPO: &str = "conformance";

fn changeset(parents: Vec<ChangesetId>, message: &str) -> Changeset {
    let mut file_changes = BTreeMap::new();
    file_changes.insert(
        RepoPath::new("file.txt").unwrap(),
        FileChange::Tracked {
            content: BlobId::hash(message.as_bytes()),
            kind: FileType::Regular,
            size: 1,
            copy_from: None,
        },
    );
    Changeset {
        repo: REPO.into(),
        parents,
        author: Identity::new("t", "t@example.com"),
        author_date: Timestamp::new(0, 0),
        committer: None,
        committer_date: None,
        message: message.into(),
        file_changes,
        extra: BTreeMap::new(),
    }
}

/// Changesets are content-addressed, idempotent, and track generation.
pub async fn changesets<S: MetadataStore>(s: &S) {
    let root = changeset(vec![], "root");
    let root_id = s.put_changeset(&root).await.unwrap();
    assert_eq!(root_id, root.id());
    assert_eq!(s.put_changeset(&root).await.unwrap(), root_id, "idempotent");
    assert_eq!(
        s.get_changeset(REPO, root_id).await.unwrap().as_ref(),
        Some(&root)
    );
    assert_eq!(s.generation(REPO, root_id).await.unwrap(), Some(0));

    let child = changeset(vec![root_id], "child");
    let child_id = s.put_changeset(&child).await.unwrap();
    assert_eq!(s.generation(REPO, child_id).await.unwrap(), Some(1));

    let unknown = ChangesetId::from_bytes([7; 32]);
    assert!(s.get_changeset(REPO, unknown).await.unwrap().is_none());
    assert!(
        s.get_changeset("other-repo", root_id)
            .await
            .unwrap()
            .is_none(),
        "repo-scoped"
    );

    let orphan = changeset(vec![unknown], "orphan");
    assert!(
        matches!(s.put_changeset(&orphan).await, Err(StoreError::UnknownParent(_, p)) if p == unknown)
    );

    // A batch may name parents earlier in it and repeat stored changesets.
    let a = changeset(vec![child_id], "batch a");
    let b = changeset(vec![a.id()], "batch b");
    let merge = changeset(vec![b.id(), root_id], "batch merge");
    s.put_changesets(&[child, a, b.clone(), merge.clone()])
        .await
        .unwrap();
    assert_eq!(s.generation(REPO, b.id()).await.unwrap(), Some(3));
    assert_eq!(s.generation(REPO, merge.id()).await.unwrap(), Some(4));
    assert_eq!(
        s.get_changeset(REPO, merge.id()).await.unwrap().as_ref(),
        Some(&merge)
    );
    assert!(matches!(
        s.put_changesets(&[changeset(vec![unknown], "batched orphan")]).await,
        Err(StoreError::UnknownParent(_, p)) if p == unknown
    ));
}

/// Refs: policy, create-once, CAS semantics, listing.
pub async fn refs<S: MetadataStore>(s: &S) {
    let a = ChangesetId::from_bytes([1; 32]);
    let b = ChangesetId::from_bytes([2; 32]);

    assert!(matches!(
        s.create_ref(REPO, "feature/x", a).await,
        Err(StoreError::RefNameNotAllowed(_))
    ));
    assert!(s.get_ref(REPO, "main").await.unwrap().is_none());

    let st = s.create_ref(REPO, "main", a).await.unwrap();
    assert_eq!(st.target, a);
    assert_eq!(st.version, 0);
    assert!(matches!(
        s.create_ref(REPO, "main", a).await,
        Err(StoreError::RefExists(_))
    ));

    assert!(matches!(
        s.move_ref(REPO, "main", 5, b).await,
        Err(StoreError::CasMismatch {
            expected: 5,
            actual: 0
        })
    ));
    let st = s.move_ref(REPO, "main", 0, b).await.unwrap();
    assert_eq!(st.target, b);
    assert_eq!(st.version, 1);
    assert_eq!(s.get_ref(REPO, "main").await.unwrap(), Some(st));
    assert!(matches!(
        s.move_ref(REPO, "nope", 0, b).await,
        Err(StoreError::RefNotFound(_))
    ));

    s.create_ref(REPO, "release/1.0", a).await.unwrap();
    let names: Vec<_> = s
        .list_refs(REPO)
        .await
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["main", "release/1.0"]);
    assert_eq!(s.list_refs("other-repo").await.unwrap().len(), 0);
}

/// Derived-data pointers are keyed by (repo, type, version, changeset).
pub async fn derived<S: MetadataStore>(s: &S) {
    let cs = ChangesetId::from_bytes([3; 32]);
    assert!(
        s.get_derived(REPO, "root_tree", 1, cs)
            .await
            .unwrap()
            .is_none()
    );
    s.put_derived(REPO, "root_tree", 1, cs, vec![9, 9])
        .await
        .unwrap();
    assert_eq!(
        s.get_derived(REPO, "root_tree", 1, cs).await.unwrap(),
        Some(vec![9, 9])
    );
    let other = ChangesetId::from_bytes([4; 32]);
    s.put_derived_many(
        REPO,
        vec![
            ("root_tree".into(), 1, other, vec![1]),
            ("skip_list".into(), 1, other, vec![2]),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        s.get_derived(REPO, "skip_list", 1, other).await.unwrap(),
        Some(vec![2])
    );
    assert_eq!(
        s.get_derived(REPO, "root_tree", 1, other).await.unwrap(),
        Some(vec![1])
    );
    assert!(
        s.get_derived(REPO, "root_tree", 2, cs)
            .await
            .unwrap()
            .is_none(),
        "versioned"
    );
    assert!(
        s.get_derived(REPO, "blame", 1, cs).await.unwrap().is_none(),
        "typed"
    );
}

/// Ingest checkpoints are per repository and idempotent: the first record of
/// a git sha wins.
pub async fn checkpoints<S: MetadataStore>(s: &S) {
    assert_eq!(s.ingest_checkpoints(REPO).await.unwrap(), vec![]);
    let (a, b) = (
        ChangesetId::from_bytes([1; 32]),
        ChangesetId::from_bytes([2; 32]),
    );
    s.put_ingest_checkpoints(REPO, &[(vec![0xaa; 20], a), (vec![0xbb; 20], b)])
        .await
        .unwrap();
    s.put_ingest_checkpoints(REPO, &[(vec![0xaa; 20], b)])
        .await
        .unwrap();
    assert_eq!(
        s.ingest_checkpoints(REPO).await.unwrap(),
        vec![(vec![0xaa; 20], a), (vec![0xbb; 20], b)]
    );
    assert_eq!(
        s.ingest_checkpoints("other").await.unwrap(),
        vec![],
        "scoped by repo"
    );
}

/// Blobs and trees are content-addressed and idempotent.
pub async fn objects<S: ObjectStore>(s: &S) {
    let data = Bytes::from_static(b"hello, scion");
    let id = s.put_blob(data.clone()).await.unwrap();
    assert_eq!(id, BlobId::hash(&data));
    assert_eq!(s.put_blob(data.clone()).await.unwrap(), id);
    assert_eq!(s.get_blob(id).await.unwrap(), Some(data));

    let missing = BlobId::hash(b"missing");
    assert!(s.get_blob(missing).await.unwrap().is_none());
    assert_eq!(
        s.has_blobs(&[id, missing, id]).await.unwrap(),
        [true, false, true]
    );

    let mut entries = BTreeMap::new();
    entries.insert(
        PathComponent::new("f").unwrap(),
        TreeEntry::Blob {
            id,
            kind: FileType::Regular,
            size: 12,
        },
    );
    let tree = Tree { entries };
    let tid = s.put_tree(&tree).await.unwrap();
    assert_eq!(tid, tree.id());
    assert_eq!(s.put_tree(&tree).await.unwrap(), tid);
    assert_eq!(s.get_tree(tid).await.unwrap(), Some(tree));
    assert!(s.get_tree(Tree::empty().id()).await.unwrap().is_none());
}

/// Instantiate the conformance suite as `#[tokio::test]`s in a module named
/// `$mod`. `$ctor` is an expression evaluating to a future that yields a
/// fresh, empty store; it is evaluated once per test.
///
/// Use [`metadata_conformance_tests!`](crate::metadata_conformance_tests) or
/// [`object_conformance_tests!`](crate::object_conformance_tests) for a store
/// that implements only one of the two traits.
#[macro_export]
macro_rules! conformance_tests {
    ($mod:ident, $ctor:expr) => {
        #[cfg(test)]
        mod $mod {
            #[allow(unused_imports)]
            use super::*;

            #[tokio::test]
            async fn changesets() {
                $crate::conformance::changesets(&$ctor.await).await;
            }
            #[tokio::test]
            async fn refs() {
                $crate::conformance::refs(&$ctor.await).await;
            }
            #[tokio::test]
            async fn derived() {
                $crate::conformance::derived(&$ctor.await).await;
            }
            #[tokio::test]
            async fn checkpoints() {
                $crate::conformance::checkpoints(&$ctor.await).await;
            }
            #[tokio::test]
            async fn objects() {
                $crate::conformance::objects(&$ctor.await).await;
            }
        }
    };
}

/// [`MetadataStore`] half of [`conformance_tests!`](crate::conformance_tests).
#[macro_export]
macro_rules! metadata_conformance_tests {
    ($mod:ident, $ctor:expr) => {
        #[cfg(test)]
        mod $mod {
            #[allow(unused_imports)]
            use super::*;

            #[tokio::test]
            async fn changesets() {
                $crate::conformance::changesets(&$ctor.await).await;
            }
            #[tokio::test]
            async fn refs() {
                $crate::conformance::refs(&$ctor.await).await;
            }
            #[tokio::test]
            async fn derived() {
                $crate::conformance::derived(&$ctor.await).await;
            }
            #[tokio::test]
            async fn checkpoints() {
                $crate::conformance::checkpoints(&$ctor.await).await;
            }
        }
    };
}

/// [`ObjectStore`] half of [`conformance_tests!`](crate::conformance_tests).
#[macro_export]
macro_rules! object_conformance_tests {
    ($mod:ident, $ctor:expr) => {
        #[cfg(test)]
        mod $mod {
            #[allow(unused_imports)]
            use super::*;

            #[tokio::test]
            async fn objects() {
                $crate::conformance::objects(&$ctor.await).await;
            }
        }
    };
}
