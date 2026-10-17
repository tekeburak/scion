//! `RootTree`: the directory tree of a changeset.
//!
//! Derived by applying the changeset's `file_changes` to the first parent's
//! tree. Only the tree nodes on a changed path are rewritten: the cost is
//! proportional to the number of distinct directories touched by the change
//! (bounded by `changed files × depth`), never to the size of the repository.

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream};
use scion_store::ObjectStore;
use scion_types::{
    BlobId, Changeset, FileChange, FileType, PathComponent, Tree, TreeEntry, TreeId,
};

use crate::{Derivable, DeriveError, Deriver, Result};

/// The root tree id of a changeset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootTree(pub TreeId);

#[async_trait]
impl Derivable for RootTree {
    const NAME: &'static str = "root_tree";
    const VERSION: u32 = 1;

    async fn derive(ctx: &Deriver<'_>, cs: &Changeset, parents: &[Self]) -> Result<Self> {
        let base = parents.first().map(|p| p.0);
        Ok(Self(
            apply_changes(ctx.objects, base, &cs.file_changes).await?,
        ))
    }

    fn to_pointer(&self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }

    fn from_pointer(bytes: &[u8]) -> Option<Self> {
        TreeId::try_from_slice(bytes).ok().map(Self)
    }
}

impl RootTree {
    /// The tree entry at `path`, or `None` if the path does not exist.
    pub async fn lookup(
        &self,
        objects: &dyn ObjectStore,
        path: &scion_types::RepoPath,
    ) -> Result<Option<TreeEntry>> {
        let mut tree = objects
            .get_tree(self.0)
            .await?
            .ok_or(DeriveError::MissingTree(self.0))?;
        let components: Vec<PathComponent> = path.components().collect();
        let (last, dirs) = components.split_last().expect("paths are non-empty");
        for component in dirs {
            match tree.entries.get(component) {
                Some(TreeEntry::Tree { id }) => {
                    tree = objects
                        .get_tree(*id)
                        .await?
                        .ok_or(DeriveError::MissingTree(*id))?;
                }
                _ => return Ok(None),
            }
        }
        Ok(tree.entries.get(last).cloned())
    }
}

/// What a change does to the entry at a path.
enum Leaf {
    Delete,
    Set(BlobId, FileType, u64),
}

/// A set of changes arranged by directory.
#[derive(Default)]
struct Node {
    leaf: Option<Leaf>,
    children: BTreeMap<PathComponent, Node>,
}

fn build_trie(changes: &BTreeMap<scion_types::RepoPath, FileChange>) -> Node {
    let mut root = Node::default();
    for (path, change) in changes {
        let mut node = &mut root;
        for component in path.components() {
            node = node.children.entry(component).or_default();
        }
        node.leaf = Some(match change {
            FileChange::Deleted => Leaf::Delete,
            FileChange::Tracked {
                content,
                kind,
                size,
                ..
            } => Leaf::Set(*content, *kind, *size),
        });
    }
    root
}

/// Most object writes a derivation keeps in flight at once.
pub(crate) const WRITE_CONCURRENCY: usize = 16;

/// Apply `changes` to the tree `base` (`None` = empty), returning the new root.
///
/// Tree ids are computed locally, so every rewritten directory is written in
/// one concurrent batch at the end instead of one round trip per level.
/// Nothing points at the new root until the caller records it, so a failed
/// batch leaves no reachable hole.
async fn apply_changes(
    objects: &dyn ObjectStore,
    base: Option<TreeId>,
    changes: &BTreeMap<scion_types::RepoPath, FileChange>,
) -> Result<TreeId> {
    let trie = build_trie(changes);
    let mut rewritten = Vec::new();
    let root = if let Some(id) = apply_node(objects, base, &trie, &mut rewritten).await? {
        id
    } else {
        let empty = Tree::empty();
        let id = empty.id();
        rewritten.push(empty);
        id
    };
    let puts: Vec<_> = rewritten
        .iter()
        .map(|tree| objects.put_tree(tree))
        .collect();
    stream::iter(puts)
        .buffer_unordered(WRITE_CONCURRENCY)
        .try_for_each(|_| async { Ok(()) })
        .await?;
    Ok(root)
}

/// Rewrite one directory, appending every new tree to `rewritten`. Returns
/// `None` when the directory ends up empty so the parent drops the entry
/// (empty directories do not exist in trees).
async fn apply_node(
    objects: &dyn ObjectStore,
    existing: Option<TreeId>,
    node: &Node,
    rewritten: &mut Vec<Tree>,
) -> Result<Option<TreeId>> {
    let mut tree = match existing {
        Some(id) => objects
            .get_tree(id)
            .await?
            .ok_or(DeriveError::MissingTree(id))?,
        None => Tree::empty(),
    };

    for (name, child) in &node.children {
        if let Some(leaf) = &child.leaf {
            match leaf {
                // A delete names a file; it never removes a directory.
                Leaf::Delete => {
                    if matches!(tree.entries.get(name), Some(TreeEntry::Blob { .. })) {
                        tree.entries.remove(name);
                    }
                }
                // A file replaces whatever was there, directory included.
                Leaf::Set(id, kind, size) => {
                    tree.entries.insert(
                        name.clone(),
                        TreeEntry::Blob {
                            id: *id,
                            kind: *kind,
                            size: *size,
                        },
                    );
                }
            }
        }
        if !child.children.is_empty() {
            let sub = match tree.entries.get(name) {
                Some(TreeEntry::Tree { id }) => Some(*id),
                _ => None,
            };
            match Box::pin(apply_node(objects, sub, child, rewritten)).await? {
                // A directory replaces whatever was there, file included.
                Some(id) => {
                    tree.entries.insert(name.clone(), TreeEntry::Tree { id });
                }
                // Became empty: drop the directory. A file of that name is
                // untouched (the change only named paths beneath it).
                None => {
                    if sub.is_some() {
                        tree.entries.remove(name);
                    }
                }
            }
        }
    }

    if tree.is_empty() {
        return Ok(None);
    }
    let id = tree.id();
    rewritten.push(tree);
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use scion_store::{MemoryStore, MetadataStore, ObjectStore};
    use scion_types::{ChangesetId, RepoPath};

    use super::*;
    use crate::test_util::{Counting, Flat, REPO, arb_changes, changeset, materialize, tracked};

    // ---- naive reference ------------------------------------------------

    /// Apply changes to a flat map.
    fn apply_flat(mut flat: Flat, changes: &BTreeMap<RepoPath, FileChange>) -> Flat {
        for (path, change) in changes {
            // A directory replaced by a file loses its contents; a file replaced
            // by a directory loses itself.
            let prefix = format!("{path}/");
            match change {
                FileChange::Deleted => {
                    flat.remove(path);
                }
                FileChange::Tracked {
                    content,
                    kind,
                    size,
                    ..
                } => {
                    flat.retain(|p, _| !p.as_str().starts_with(&prefix));
                    for ancestor in std::iter::successors(path.parent(), RepoPath::parent) {
                        flat.remove(&ancestor);
                    }
                    flat.insert(path.clone(), (*content, *kind, *size));
                }
            }
        }
        flat
    }

    /// Build a tree from scratch from a flat map.
    async fn tree_from_flat(objects: &dyn ObjectStore, flat: &Flat) -> TreeId {
        #[derive(Default)]
        struct Dir {
            files: BTreeMap<PathComponent, (BlobId, FileType, u64)>,
            dirs: BTreeMap<PathComponent, Dir>,
        }
        async fn write(objects: &dyn ObjectStore, dir: &Dir) -> TreeId {
            let mut tree = Tree::empty();
            for (name, (id, kind, size)) in &dir.files {
                tree.entries.insert(
                    name.clone(),
                    TreeEntry::Blob {
                        id: *id,
                        kind: *kind,
                        size: *size,
                    },
                );
            }
            for (name, sub) in &dir.dirs {
                let id = Box::pin(write(objects, sub)).await;
                tree.entries.insert(name.clone(), TreeEntry::Tree { id });
            }
            objects.put_tree(&tree).await.unwrap()
        }

        let mut root = Dir::default();
        for (path, file) in flat {
            let comps: Vec<_> = path.components().collect();
            let (last, dirs) = comps.split_last().unwrap();
            let mut dir = &mut root;
            for component in dirs {
                dir = dir.dirs.entry(component.clone()).or_default();
            }
            dir.files.insert(last.clone(), *file);
        }
        write(objects, &root).await
    }

    // ---- tests ----------------------------------------------------------

    #[tokio::test]
    async fn root_changeset_builds_tree_and_empty_dirs_are_pruned() {
        let store = MemoryStore::with_repo(REPO);
        let mut changes = BTreeMap::new();
        changes.insert(RepoPath::new("a/b/c.txt").unwrap(), tracked(b"1"));
        changes.insert(RepoPath::new("a/d.txt").unwrap(), tracked(b"2"));
        changes.insert(RepoPath::new("e.txt").unwrap(), tracked(b"3"));
        let root = store
            .put_changeset(&changeset(vec![], changes.clone(), "root"))
            .await
            .unwrap();

        let d = Deriver::new(REPO, &store, &store);
        let RootTree(t1) = d.derive::<RootTree>(root).await.unwrap();
        assert_eq!(
            materialize(&store, t1).await,
            apply_flat(Flat::new(), &changes)
        );

        // Delete the only file under a/b → a/b vanishes; a stays.
        let mut del = BTreeMap::new();
        del.insert(RepoPath::new("a/b/c.txt").unwrap(), FileChange::Deleted);
        let child = store
            .put_changeset(&changeset(vec![root], del, "del"))
            .await
            .unwrap();
        let RootTree(t2) = d.derive::<RootTree>(child).await.unwrap();
        let flat = materialize(&store, t2).await;
        assert_eq!(flat.len(), 2);
        let a = store.get_tree(t2).await.unwrap().unwrap();
        let Some(TreeEntry::Tree { id }) = a.entries.get(&PathComponent::new("a").unwrap()) else {
            panic!()
        };
        let a = store.get_tree(*id).await.unwrap().unwrap();
        assert!(!a.entries.contains_key(&PathComponent::new("b").unwrap()));
    }

    #[tokio::test]
    async fn rewritten_nodes_bounded_by_depth_not_file_count() {
        // N files spread over a wide tree; changing one file at depth D must
        // rewrite exactly D+1 tree nodes (the directories on its path).
        const WIDTH: usize = 50;
        const DEPTH: usize = 4; // path has DEPTH directory components
        let store = MemoryStore::with_repo(REPO);

        let mut changes = BTreeMap::new();
        for i in 0..WIDTH {
            for j in 0..WIDTH {
                // d0_i/d1_j/d2/d3/file → directories: d0_i, d1_j, d2, d3
                let p = format!("d0_{i}/d1_{j}/d2/d3/f_{i}_{j}.txt");
                changes.insert(
                    RepoPath::new(p).unwrap(),
                    tracked(format!("{i}-{j}").as_bytes()),
                );
            }
        }
        let n_files = changes.len();
        assert_eq!(n_files, WIDTH * WIDTH);
        let root = store
            .put_changeset(&changeset(vec![], changes, "big"))
            .await
            .unwrap();
        Deriver::new(REPO, &store, &store)
            .derive::<RootTree>(root)
            .await
            .unwrap();

        let mut one = BTreeMap::new();
        one.insert(
            RepoPath::new("d0_7/d1_9/d2/d3/f_7_9.txt").unwrap(),
            tracked(b"changed"),
        );
        let child = store
            .put_changeset(&changeset(vec![root], one, "one"))
            .await
            .unwrap();

        let counting = Counting::new(&store);
        Deriver::new(REPO, &store, &counting)
            .derive::<RootTree>(child)
            .await
            .unwrap();
        let puts = counting.tree_puts();
        assert_eq!(
            puts,
            DEPTH + 1,
            "rewrote {puts} nodes for one change in a {n_files}-file tree"
        );
    }

    #[tokio::test]
    async fn diamond_derives_each_ancestor_once_and_caches() {
        let store = MemoryStore::with_repo(REPO);
        let single = |path: &str| {
            let mut changes = BTreeMap::new();
            changes.insert(RepoPath::new(path).unwrap(), tracked(path.as_bytes()));
            changes
        };
        let root = store
            .put_changeset(&changeset(vec![], single("x"), "root"))
            .await
            .unwrap();
        let left = store
            .put_changeset(&changeset(vec![root], single("l"), "left"))
            .await
            .unwrap();
        let right = store
            .put_changeset(&changeset(vec![root], single("r"), "right"))
            .await
            .unwrap();
        let merge = store
            .put_changeset(&changeset(vec![left, right], BTreeMap::new(), "merge"))
            .await
            .unwrap();

        let counting = Counting::new(&store);
        let d = Deriver::new(REPO, &store, &counting);
        let RootTree(t) = d.derive::<RootTree>(merge).await.unwrap();
        // root, left, right, merge: one root-level tree each.
        assert_eq!(counting.tree_puts(), 4);
        // Merge follows the first parent: has x and l, not r.
        let flat = materialize(&store, t).await;
        assert_eq!(
            flat.keys().map(RepoPath::as_str).collect::<Vec<_>>(),
            ["l", "x"]
        );

        // Everything is cached now: a second derive does no work.
        for cs in [root, left, right, merge] {
            assert!(d.fetch::<RootTree>(cs).await.unwrap().is_some());
        }
        let RootTree(t2) = d.derive::<RootTree>(merge).await.unwrap();
        assert_eq!(t2, t);
        assert_eq!(counting.tree_puts(), 4);
    }

    #[tokio::test]
    async fn missing_changeset_is_an_error() {
        let store = MemoryStore::with_repo(REPO);
        let d = Deriver::new(REPO, &store, &store);
        let err = d
            .derive::<RootTree>(ChangesetId::from_bytes([9; 32]))
            .await
            .unwrap_err();
        assert!(matches!(err, DeriveError::MissingChangeset(_)));
    }

    // ---- property test: incremental derivation ≡ from-scratch build ------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn incremental_equals_naive(steps in prop::collection::vec(arb_changes(), 1..8)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let store = MemoryStore::with_repo(REPO);
                let d = Deriver::new(REPO, &store, &store);
                let mut parent: Option<ChangesetId> = None;
                let mut flat = Flat::new();
                for (i, changes) in steps.into_iter().enumerate() {
                    let cs = changeset(parent.into_iter().collect(), changes.clone(), &i.to_string());
                    let id = store.put_changeset(&cs).await.unwrap();
                    let RootTree(derived) = d.derive::<RootTree>(id).await.unwrap();

                    flat = apply_flat(flat, &changes);
                    let expected = tree_from_flat(&store, &flat).await;
                    prop_assert_eq!(derived, expected, "step {}", i);
                    prop_assert_eq!(materialize(&store, derived).await, flat.clone());
                    parent = Some(id);
                }
                Ok(())
            })?;
        }
    }
}
