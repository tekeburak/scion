//! `PathHistory`: for every path, the changeset that last changed it.
//!
//! The structure mirrors the root tree: a history node per directory, with
//! one entry per tree entry recording `last_changed` — the most recent
//! first-parent ancestor (inclusive) in which that file's content, or
//! anything beneath that directory, differed. Nodes are stored as blobs.
//!
//! Derivation is driven by the *diff* between the changeset's root tree and
//! its first parent's root tree: subtrees with equal ids reuse the parent's
//! history node pointer unchanged, so only nodes on actually-changed paths are
//! rewritten. No-op changes (deleting a file that does not exist) therefore
//! leave the history untouched, because they leave the tree untouched.
//!
//! "Which changesets touched `path`?" is answered by repeatedly looking up
//! `path` in the history of `first_parent(last_changed)`; each hop costs one
//! path lookup, so a path's log costs O(changes to that path × depth), never
//! a scan of the changeset log.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use scion_store::ObjectStore;
use scion_types::canonical::{Reader, Writer};
use scion_types::{
    BlobId, Changeset, ChangesetId, PathComponent, RepoPath, Tree, TreeEntry, TreeId,
};

use crate::root_tree::WRITE_CONCURRENCY;
use crate::{Derivable, DeriveError, Deriver, Result, RootTree};

/// The root history node of a changeset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathHistory(pub BlobId);

/// History of one tree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryEntry {
    File {
        last_changed: ChangesetId,
    },
    Dir {
        last_changed: ChangesetId,
        node: BlobId,
    },
}

impl HistoryEntry {
    /// The most recent first-parent ancestor that changed this entry.
    #[must_use]
    pub fn last_changed(&self) -> ChangesetId {
        match self {
            Self::File { last_changed } | Self::Dir { last_changed, .. } => *last_changed,
        }
    }
}

/// One directory's worth of history entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryNode {
    pub entries: BTreeMap<PathComponent, HistoryEntry>,
}

const ENTRY_FILE: u8 = 0;
const ENTRY_DIR: u8 = 1;

impl HistoryNode {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.seq(self.entries.iter(), |w, (name, entry)| {
            w.str(name.as_str());
            match entry {
                HistoryEntry::File { last_changed } => {
                    w.u8(ENTRY_FILE);
                    w.id(last_changed.as_bytes());
                }
                HistoryEntry::Dir { last_changed, node } => {
                    w.u8(ENTRY_DIR);
                    w.id(last_changed.as_bytes());
                    w.id(node.as_bytes());
                }
            }
        });
        w.into_vec()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader::new(bytes);
        let n = r.seq_len().ok()?;
        let mut entries = BTreeMap::new();
        for _ in 0..n {
            let name = PathComponent::new(r.str().ok()?).ok()?;
            let entry = match r.u8().ok()? {
                ENTRY_FILE => HistoryEntry::File {
                    last_changed: ChangesetId::from_bytes(r.id().ok()?),
                },
                ENTRY_DIR => HistoryEntry::Dir {
                    last_changed: ChangesetId::from_bytes(r.id().ok()?),
                    node: BlobId::from_bytes(r.id().ok()?),
                },
                _ => return None,
            };
            entries.insert(name, entry);
        }
        r.finish().ok()?;
        Some(Self { entries })
    }

    async fn load(objects: &dyn ObjectStore, id: BlobId) -> Result<Self> {
        let bytes = objects
            .get_blob(id)
            .await?
            .ok_or(DeriveError::MissingObject(id))?;
        Self::decode(&bytes).ok_or(DeriveError::CorruptObject(id))
    }
}

#[async_trait]
impl Derivable for PathHistory {
    const NAME: &'static str = "path_history";
    const VERSION: u32 = 1;

    async fn derive(ctx: &Deriver<'_>, cs: &Changeset, parents: &[Self]) -> Result<Self> {
        let cs_id = cs.id();
        let RootTree(new_root) = ctx.derive::<RootTree>(cs_id).await?;
        let old = match (cs.first_parent(), parents.first()) {
            (Some(parent), Some(PathHistory(history))) => {
                let RootTree(tree) = ctx.derive::<RootTree>(parent).await?;
                Some((tree, *history))
            }
            _ => None,
        };
        let mut rewritten = Vec::new();
        let root = rebuild(ctx.objects, new_root, old, cs_id, &mut rewritten).await?;
        // Node ids are content hashes, so every rewritten node is written in
        // one concurrent batch; nothing points at them until the pointer is
        // recorded.
        stream::iter(rewritten.into_iter().map(|node| ctx.objects.put_blob(node)))
            .buffer_unordered(WRITE_CONCURRENCY)
            .try_for_each(|_| async { Ok(()) })
            .await?;
        Ok(Self(root))
    }

    fn to_pointer(&self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }

    fn from_pointer(bytes: &[u8]) -> Option<Self> {
        BlobId::try_from_slice(bytes).ok().map(Self)
    }
}

/// Build the history node for `new_tree` given the parent's `(tree, history)`
/// for the same directory, attributing every difference to `cs`. Every new
/// node's encoding is appended to `rewritten` for the caller to store.
async fn rebuild(
    objects: &dyn ObjectStore,
    new_tree: TreeId,
    old: Option<(TreeId, BlobId)>,
    cs: ChangesetId,
    rewritten: &mut Vec<Bytes>,
) -> Result<BlobId> {
    if let Some((old_tree, old_history)) = old {
        if old_tree == new_tree {
            return Ok(old_history);
        }
    }

    let new = load_tree(objects, new_tree).await?;
    let (old_tree, old_history) = match old {
        Some((t, h)) => (
            load_tree(objects, t).await?,
            HistoryNode::load(objects, h).await?,
        ),
        None => (Tree::empty(), HistoryNode::default()),
    };

    let mut node = HistoryNode::default();
    for (name, entry) in &new.entries {
        if old_tree.entries.get(name) == Some(entry) {
            let unchanged = old_history
                .entries
                .get(name)
                .ok_or(DeriveError::CorruptObject(old.expect("old exists").1))?;
            node.entries.insert(name.clone(), *unchanged);
            continue;
        }
        let history = match entry {
            TreeEntry::Blob { .. } => HistoryEntry::File { last_changed: cs },
            TreeEntry::Tree { id } => {
                let old_sub = match (old_tree.entries.get(name), old_history.entries.get(name)) {
                    (Some(TreeEntry::Tree { id: t }), Some(HistoryEntry::Dir { node, .. })) => {
                        Some((*t, *node))
                    }
                    _ => None,
                };
                let sub = Box::pin(rebuild(objects, *id, old_sub, cs, rewritten)).await?;
                HistoryEntry::Dir {
                    last_changed: cs,
                    node: sub,
                }
            }
        };
        node.entries.insert(name.clone(), history);
    }
    let bytes = Bytes::from(node.encode());
    let id = BlobId::hash(&bytes);
    rewritten.push(bytes);
    Ok(id)
}

async fn load_tree(objects: &dyn ObjectStore, id: TreeId) -> Result<Tree> {
    objects
        .get_tree(id)
        .await?
        .ok_or(DeriveError::MissingTree(id))
}

impl PathHistory {
    /// The history entry at `path`, or `None` if the path does not exist.
    pub async fn lookup(
        &self,
        objects: &dyn ObjectStore,
        path: &RepoPath,
    ) -> Result<Option<HistoryEntry>> {
        let mut node = HistoryNode::load(objects, self.0).await?;
        let components: Vec<PathComponent> = path.components().collect();
        let (last, dirs) = components.split_last().expect("paths are non-empty");
        for component in dirs {
            match node.entries.get(component) {
                Some(HistoryEntry::Dir { node: id, .. }) => {
                    node = HistoryNode::load(objects, *id).await?;
                }
                _ => return Ok(None),
            }
        }
        Ok(node.entries.get(last).copied())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use scion_store::{MemoryStore, MetadataStore};
    use scion_types::FileChange;

    use super::*;
    use crate::test_util::{Counting, Flat, REPO, arb_changes, changeset, materialize, tracked};

    // ---- naive reference ------------------------------------------------

    /// Everything at or beneath `path` in a flat view.
    fn subtree<'a>(
        flat: &'a Flat,
        path: &RepoPath,
    ) -> Vec<(&'a RepoPath, &'a (BlobId, scion_types::FileType, u64))> {
        let prefix = format!("{path}/");
        flat.iter()
            .filter(|(p, _)| *p == path || p.as_str().starts_with(&prefix))
            .collect()
    }

    /// For a first-parent chain `ids[0..=k]` with flat views `flats`, the
    /// latest `j <= k` whose subtree at `path` differs from `j-1`'s.
    fn naive_last_changed(
        ids: &[ChangesetId],
        flats: &[Flat],
        k: usize,
        path: &RepoPath,
    ) -> Option<ChangesetId> {
        if subtree(&flats[k], path).is_empty() {
            return None;
        }
        let empty = Flat::new();
        (0..=k)
            .rev()
            .find(|&j| {
                let before = if j == 0 { &empty } else { &flats[j - 1] };
                subtree(&flats[j], path) != subtree(before, path)
            })
            .map(|j| ids[j])
    }

    /// Every path that exists in `flat`, files and directories.
    fn all_paths(flat: &Flat) -> Vec<(RepoPath, bool)> {
        let mut out = std::collections::BTreeMap::new();
        for path in flat.keys() {
            out.insert(path.clone(), false);
            for dir in std::iter::successors(path.parent(), RepoPath::parent) {
                out.insert(dir, true);
            }
        }
        out.into_iter().collect()
    }

    // ---- tests ----------------------------------------------------------

    #[test]
    fn node_encoding_roundtrips() {
        let mut node = HistoryNode::default();
        node.entries.insert(
            PathComponent::new("f").unwrap(),
            HistoryEntry::File {
                last_changed: ChangesetId::from_bytes([1; 32]),
            },
        );
        node.entries.insert(
            PathComponent::new("d").unwrap(),
            HistoryEntry::Dir {
                last_changed: ChangesetId::from_bytes([2; 32]),
                node: BlobId::from_bytes([3; 32]),
            },
        );
        assert_eq!(HistoryNode::decode(&node.encode()), Some(node));
        assert_eq!(HistoryNode::decode(b"garbage"), None);
    }

    #[tokio::test]
    async fn unchanged_tree_reuses_history_pointer() {
        let store = MemoryStore::new();
        let d = Deriver::new(REPO, &store, &store);
        let mut c = BTreeMap::new();
        c.insert(RepoPath::new("a/f").unwrap(), tracked(b"1"));
        let root = store
            .put_changeset(&changeset(vec![], c, "root"))
            .await
            .unwrap();
        // Deleting a file that does not exist changes nothing.
        let mut noop = BTreeMap::new();
        noop.insert(RepoPath::new("a/missing").unwrap(), FileChange::Deleted);
        let child = store
            .put_changeset(&changeset(vec![root], noop, "noop"))
            .await
            .unwrap();

        let h0 = d.derive::<PathHistory>(root).await.unwrap();
        let h1 = d.derive::<PathHistory>(child).await.unwrap();
        assert_eq!(h0, h1);
        let entry = h1
            .lookup(&store, &RepoPath::new("a/f").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.last_changed(), root);
        assert!(
            h1.lookup(&store, &RepoPath::new("a/missing").unwrap())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            h1.lookup(&store, &RepoPath::new("a/f/x").unwrap())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn merge_follows_first_parent() {
        let store = MemoryStore::new();
        let d = Deriver::new(REPO, &store, &store);
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

        let h = d.derive::<PathHistory>(merge).await.unwrap();
        assert_eq!(h, d.derive::<PathHistory>(left).await.unwrap());
        let last = |p: &str| {
            let store = &store;
            let p = RepoPath::new(p).unwrap();
            async move { h.lookup(store, &p).await.unwrap().map(|e| e.last_changed()) }
        };
        assert_eq!(last("x").await, Some(root));
        assert_eq!(last("l").await, Some(left));
        assert_eq!(last("r").await, None);
    }

    #[tokio::test]
    async fn rewritten_history_nodes_bounded_by_depth() {
        const WIDTH: usize = 50;
        const DEPTH: usize = 4;
        let store = MemoryStore::new();
        let mut changes = BTreeMap::new();
        for i in 0..WIDTH {
            for j in 0..WIDTH {
                let p = format!("d0_{i}/d1_{j}/d2/d3/f_{i}_{j}.txt");
                changes.insert(
                    RepoPath::new(p).unwrap(),
                    tracked(format!("{i}-{j}").as_bytes()),
                );
            }
        }
        let root = store
            .put_changeset(&changeset(vec![], changes, "big"))
            .await
            .unwrap();
        Deriver::new(REPO, &store, &store)
            .derive::<PathHistory>(root)
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
        let h = Deriver::new(REPO, &store, &counting)
            .derive::<PathHistory>(child)
            .await
            .unwrap();
        assert_eq!(counting.blob_puts(), DEPTH + 1, "history nodes rewritten");
        assert_eq!(counting.tree_puts(), DEPTH + 1, "tree nodes rewritten");

        let last = |p: &str| {
            let store = &store;
            let p = RepoPath::new(p).unwrap();
            async move { h.lookup(store, &p).await.unwrap().map(|e| e.last_changed()) }
        };
        assert_eq!(last("d0_7/d1_9/d2/d3/f_7_9.txt").await, Some(child));
        assert_eq!(last("d0_7/d1_9").await, Some(child));
        assert_eq!(last("d0_7/d1_8/d2/d3/f_7_8.txt").await, Some(root));
        assert_eq!(last("d0_8").await, Some(root));
    }

    // ---- property test: incremental ≡ naive scan ------------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn lookup_equals_naive(steps in prop::collection::vec(arb_changes(), 1..8)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let store = MemoryStore::new();
                let d = Deriver::new(REPO, &store, &store);
                let mut ids = Vec::new();
                let mut flats = Vec::new();
                for (i, changes) in steps.into_iter().enumerate() {
                    let parents = ids.last().copied().into_iter().collect();
                    let id = store.put_changeset(&changeset(parents, changes, &i.to_string())).await.unwrap();
                    let RootTree(tree) = d.derive::<RootTree>(id).await.unwrap();
                    ids.push(id);
                    flats.push(materialize(&store, tree).await);

                    let history = d.derive::<PathHistory>(id).await.unwrap();
                    for (path, is_dir) in all_paths(&flats[i]) {
                        let entry = history.lookup(&store, &path).await.unwrap();
                        let expected = naive_last_changed(&ids, &flats, i, &path);
                        prop_assert_eq!(entry.map(|e| e.last_changed()), expected, "step {} path {}", i, path);
                        prop_assert_eq!(matches!(entry, Some(HistoryEntry::Dir { .. })), is_dir, "kind of {}", path);
                    }
                    // A path that does not exist has no history.
                    let absent = RepoPath::new("zz/zz").unwrap();
                    prop_assert_eq!(history.lookup(&store, &absent).await.unwrap(), None);
                }
                Ok(())
            })?;
        }
    }
}
