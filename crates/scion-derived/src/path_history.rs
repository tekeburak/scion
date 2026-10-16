//! `PathHistory`: for every path, the changeset that last changed it.
//!
//! The structure mirrors the root tree: a history node per directory, with
//! one entry per tree entry recording `last_changed` — the most recent
//! first-parent ancestor (inclusive) in which that file's content, or
//! anything beneath that directory, differed.
//!
//! Derivation is driven by the *diff* between the changeset's root tree and
//! its first parent's root tree: subtrees with equal ids reuse the parent's
//! history node unchanged, so only nodes on actually-changed paths are
//! rewritten. No-op changes (deleting a file that does not exist) therefore
//! leave the history untouched, because they leave the tree untouched.
//!
//! The nodes one changeset rewrites are stored together, children before
//! parents, in pages of about `PAGE_BYTES`: one blob per page, so a
//! one-file change writes one object however deep the file is. A node is
//! named by its page and its index there ([`NodeAt`]); a node refers to a
//! child in its own page by index alone, since the page's id is not known
//! while it is being written.
//!
//! Every entry also names the node it replaced: the history node of its
//! directory at `first_parent(last_changed)`, carried unchanged while the
//! entry is. "Which changesets touched `path`?" is answered by one lookup at
//! the head and then one node read per change ([`HistoryEntry::previous`]),
//! so a path's log costs O(depth + changes to that path) object reads and no
//! metadata read, never a scan of the changeset log.

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

/// A page is closed once its nodes' encodings reach this many bytes. Small
/// enough that a page stays in the object store's memo of small blobs.
const PAGE_BYTES: usize = 16 << 10;

/// A stored history node: the page (blob) holding it and its index there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeAt {
    pub page: BlobId,
    pub index: u32,
}

/// The root history node of a changeset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathHistory(pub NodeAt);

/// History of one tree entry.
///
/// `replaced` is the history node of the entry's directory at
/// `first_parent(last_changed)`, the node `last_changed` rewrote; `None` when
/// the directory did not exist there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryEntry {
    File {
        last_changed: ChangesetId,
        replaced: Option<NodeAt>,
    },
    Dir {
        last_changed: ChangesetId,
        replaced: Option<NodeAt>,
        node: NodeAt,
    },
}

impl HistoryEntry {
    /// The most recent first-parent ancestor that changed this entry.
    #[must_use]
    pub fn last_changed(&self) -> ChangesetId {
        match self {
            Self::File { last_changed, .. } | Self::Dir { last_changed, .. } => *last_changed,
        }
    }

    /// The node this entry's last change replaced.
    #[must_use]
    pub fn replaced(&self) -> Option<NodeAt> {
        match self {
            Self::File { replaced, .. } | Self::Dir { replaced, .. } => *replaced,
        }
    }

    /// The entry `name` (this entry's own name) had at
    /// `first_parent(last_changed)`: the previous change to it, or `None` if
    /// it did not exist there. One node read.
    pub async fn previous(
        &self,
        objects: &dyn ObjectStore,
        name: &PathComponent,
    ) -> Result<Option<Self>> {
        let Some(at) = self.replaced() else {
            return Ok(None);
        };
        Ok(HistoryNode::load(objects, at)
            .await?
            .entries
            .get(name)
            .copied())
    }
}

/// One directory's worth of history entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryNode {
    pub entries: BTreeMap<PathComponent, HistoryEntry>,
}

/// An entry as written in a page: a child directory in the same page is
/// named by its index alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageEntry {
    Stored(HistoryEntry),
    Here {
        last_changed: ChangesetId,
        replaced: Option<NodeAt>,
        index: u32,
    },
}

type PageNode = BTreeMap<PathComponent, PageEntry>;

const ENTRY_FILE: u8 = 0;
const ENTRY_DIR: u8 = 1;
const ENTRY_DIR_HERE: u8 = 2;

fn write_node_at(w: &mut Writer, at: &NodeAt) {
    w.id(at.page.as_bytes());
    w.u64(u64::from(at.index));
}

fn read_node_at(r: &mut Reader<'_>) -> Option<NodeAt> {
    Some(NodeAt {
        page: BlobId::from_bytes(r.id().ok()?),
        index: u32::try_from(r.u64().ok()?).ok()?,
    })
}

fn encode_page(nodes: &[PageNode]) -> Vec<u8> {
    let mut w = Writer::new();
    w.seq(nodes.iter(), |w, node| {
        w.seq(node.iter(), |w, (name, entry)| {
            w.str(name.as_str());
            let (tag, last_changed, replaced) = match entry {
                PageEntry::Stored(HistoryEntry::File {
                    last_changed,
                    replaced,
                }) => (ENTRY_FILE, last_changed, replaced),
                PageEntry::Stored(HistoryEntry::Dir {
                    last_changed,
                    replaced,
                    ..
                }) => (ENTRY_DIR, last_changed, replaced),
                PageEntry::Here {
                    last_changed,
                    replaced,
                    ..
                } => (ENTRY_DIR_HERE, last_changed, replaced),
            };
            w.u8(tag);
            w.id(last_changed.as_bytes());
            w.opt(replaced.as_ref(), write_node_at);
            match entry {
                PageEntry::Stored(HistoryEntry::File { .. }) => {}
                PageEntry::Stored(HistoryEntry::Dir { node, .. }) => write_node_at(w, node),
                PageEntry::Here { index, .. } => w.u64(u64::from(*index)),
            }
        });
    });
    w.into_vec()
}

/// Node `index` of the page `page` whose bytes are `bytes`; `None` if the
/// page is malformed up to that node or has no such node.
fn decode_node(bytes: &[u8], page: BlobId, index: u32) -> Option<HistoryNode> {
    let mut r = Reader::new(bytes);
    let n = r.seq_len().ok()?;
    for i in 0..n {
        let mut node = HistoryNode::default();
        for _ in 0..r.seq_len().ok()? {
            let name = PathComponent::new(r.str().ok()?).ok()?;
            let tag = r.u8().ok()?;
            let last_changed = ChangesetId::from_bytes(r.id().ok()?);
            let replaced = match r.u8().ok()? {
                0 => None,
                1 => Some(read_node_at(&mut r)?),
                _ => return None,
            };
            let entry = match tag {
                ENTRY_FILE => HistoryEntry::File {
                    last_changed,
                    replaced,
                },
                ENTRY_DIR => HistoryEntry::Dir {
                    last_changed,
                    replaced,
                    node: read_node_at(&mut r)?,
                },
                ENTRY_DIR_HERE => {
                    let index = u32::try_from(r.u64().ok()?).ok()?;
                    // Children come before their parent in a page.
                    if u64::from(index) >= i {
                        return None;
                    }
                    HistoryEntry::Dir {
                        last_changed,
                        replaced,
                        node: NodeAt { page, index },
                    }
                }
                _ => return None,
            };
            node.entries.insert(name, entry);
        }
        // The page's bytes were checked against its id when read, so the
        // nodes after the wanted one need not be parsed.
        if i == u64::from(index) {
            return Some(node);
        }
    }
    None
}

impl HistoryNode {
    async fn load(objects: &dyn ObjectStore, at: NodeAt) -> Result<Self> {
        let bytes = objects
            .get_blob(at.page)
            .await?
            .ok_or(DeriveError::MissingObject(at.page))?;
        decode_node(&bytes, at.page, at.index).ok_or(DeriveError::CorruptObject(at.page))
    }
}

/// A node rewritten by the derivation in progress: already stored, or the
/// `index`th node of page number `page`.
#[derive(Debug, Clone, Copy)]
enum Built {
    Stored(NodeAt),
    New { page: usize, index: u32 },
}

/// An entry of a node being written; `dir` is set for a directory.
struct NewEntry {
    name: PathComponent,
    last_changed: ChangesetId,
    replaced: Option<NodeAt>,
    dir: Option<Built>,
}

/// The pages one derivation writes, filled in order.
#[derive(Default)]
struct Pages {
    closed: Vec<(BlobId, Bytes)>,
    open: Vec<PageNode>,
    open_bytes: usize,
}

impl Pages {
    /// Append a node whose children are `entries`; returns where it is.
    fn push(&mut self, entries: Vec<NewEntry>) -> Built {
        let page = self.closed.len();
        let mut node = PageNode::new();
        for NewEntry {
            name,
            last_changed,
            replaced,
            dir,
        } in entries
        {
            self.open_bytes += name.as_str().len() + 104;
            let entry = match dir {
                None => PageEntry::Stored(HistoryEntry::File {
                    last_changed,
                    replaced,
                }),
                Some(Built::Stored(at)) => PageEntry::Stored(HistoryEntry::Dir {
                    last_changed,
                    replaced,
                    node: at,
                }),
                Some(Built::New { page: p, index }) if p == page => PageEntry::Here {
                    last_changed,
                    replaced,
                    index,
                },
                Some(Built::New { page: p, index }) => PageEntry::Stored(HistoryEntry::Dir {
                    last_changed,
                    replaced,
                    node: NodeAt {
                        page: self.closed[p].0,
                        index,
                    },
                }),
            };
            node.insert(name, entry);
        }
        let index = u32::try_from(self.open.len()).expect("a page holds fewer than 2^32 nodes");
        self.open.push(node);
        if self.open_bytes >= PAGE_BYTES {
            self.close();
        }
        Built::New { page, index }
    }

    fn close(&mut self) {
        if !self.open.is_empty() {
            let bytes = Bytes::from(encode_page(&self.open));
            self.closed.push((BlobId::hash(&bytes), bytes));
            self.open.clear();
            self.open_bytes = 0;
        }
    }

    fn at(&self, built: Built) -> NodeAt {
        match built {
            Built::Stored(at) => at,
            Built::New { page, index } => NodeAt {
                page: self.closed[page].0,
                index,
            },
        }
    }
}

#[async_trait]
impl Derivable for PathHistory {
    const NAME: &'static str = "path_history";
    const VERSION: u32 = 3;

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
        let mut pages = Pages::default();
        let root = rebuild(ctx.objects, new_root, old, cs_id, &mut pages).await?;
        pages.close();
        let root = pages.at(root);
        // Page ids are content hashes, so every page is written in one
        // concurrent batch; nothing points at them until the pointer is
        // recorded.
        stream::iter(
            pages
                .closed
                .into_iter()
                .map(|(_, page)| ctx.objects.put_blob(page)),
        )
        .buffer_unordered(WRITE_CONCURRENCY)
        .try_for_each(|_| async { Ok(()) })
        .await?;
        Ok(Self(root))
    }

    fn to_pointer(&self) -> Vec<u8> {
        let mut pointer = self.0.page.as_bytes().to_vec();
        pointer.extend_from_slice(&self.0.index.to_be_bytes());
        pointer
    }

    fn from_pointer(bytes: &[u8]) -> Option<Self> {
        let (page, index) = bytes.split_at_checked(BlobId::LEN)?;
        Some(Self(NodeAt {
            page: BlobId::try_from_slice(page).ok()?,
            index: u32::from_be_bytes(index.try_into().ok()?),
        }))
    }
}

/// Build the history node for `new_tree` given the parent's `(tree, history)`
/// for the same directory, attributing every difference to `cs`. Every new
/// node is appended to `pages`, children first.
async fn rebuild(
    objects: &dyn ObjectStore,
    new_tree: TreeId,
    old: Option<(TreeId, NodeAt)>,
    cs: ChangesetId,
    pages: &mut Pages,
) -> Result<Built> {
    if let Some((old_tree, old_history)) = old {
        if old_tree == new_tree {
            return Ok(Built::Stored(old_history));
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

    let mut entries = Vec::with_capacity(new.entries.len());
    for (name, entry) in &new.entries {
        if old_tree.entries.get(name) == Some(entry) {
            let unchanged = old_history
                .entries
                .get(name)
                .ok_or(DeriveError::CorruptObject(old.expect("old exists").1.page))?;
            let dir = match unchanged {
                HistoryEntry::File { .. } => None,
                HistoryEntry::Dir { node, .. } => Some(Built::Stored(*node)),
            };
            entries.push(NewEntry {
                name: name.clone(),
                last_changed: unchanged.last_changed(),
                replaced: unchanged.replaced(),
                dir,
            });
            continue;
        }
        let dir = match entry {
            TreeEntry::Blob { .. } => None,
            TreeEntry::Tree { id } => {
                let old_sub = match (old_tree.entries.get(name), old_history.entries.get(name)) {
                    (Some(TreeEntry::Tree { id: t }), Some(HistoryEntry::Dir { node, .. })) => {
                        Some((*t, *node))
                    }
                    _ => None,
                };
                Some(Box::pin(rebuild(objects, *id, old_sub, cs, pages)).await?)
            }
        };
        entries.push(NewEntry {
            name: name.clone(),
            last_changed: cs,
            replaced: old.map(|(_, history)| history),
            dir,
        });
    }
    Ok(pages.push(entries))
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
                Some(HistoryEntry::Dir { node: at, .. }) => {
                    node = HistoryNode::load(objects, *at).await?;
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
    fn page_encoding_roundtrips() {
        let (a, b) = (
            ChangesetId::from_bytes([1; 32]),
            ChangesetId::from_bytes([2; 32]),
        );
        let elsewhere = NodeAt {
            page: BlobId::from_bytes([3; 32]),
            index: 7,
        };
        let before = Some(NodeAt {
            page: BlobId::from_bytes([4; 32]),
            index: 1,
        });
        let name = |s| PathComponent::new(s).unwrap();
        let file = HistoryEntry::File {
            last_changed: a,
            replaced: None,
        };
        let child = PageNode::from([(name("f"), PageEntry::Stored(file))]);
        let parent = PageNode::from([
            (
                name("d"),
                PageEntry::Here {
                    last_changed: b,
                    replaced: before,
                    index: 0,
                },
            ),
            (
                name("e"),
                PageEntry::Stored(HistoryEntry::Dir {
                    last_changed: a,
                    replaced: before,
                    node: elsewhere,
                }),
            ),
        ]);
        let bytes = encode_page(&[child, parent]);
        let page = BlobId::hash(&bytes);

        assert_eq!(
            decode_node(&bytes, page, 0).unwrap().entries[&name("f")],
            file
        );
        let parent = decode_node(&bytes, page, 1).unwrap();
        assert_eq!(
            parent.entries[&name("d")],
            HistoryEntry::Dir {
                last_changed: b,
                replaced: before,
                node: NodeAt { page, index: 0 },
            },
            "a child in the same page is named by the page being read"
        );
        assert_eq!(
            parent.entries[&name("e")],
            HistoryEntry::Dir {
                last_changed: a,
                replaced: before,
                node: elsewhere,
            }
        );
        assert_eq!(decode_node(&bytes, page, 2), None, "no such node");
        assert_eq!(decode_node(b"garbage", page, 0), None);
        let forward = encode_page(&[PageNode::from([(
            name("d"),
            PageEntry::Here {
                last_changed: b,
                replaced: None,
                index: 0,
            },
        )])]);
        assert_eq!(
            decode_node(&forward, page, 0),
            None,
            "a node names only earlier nodes of its page"
        );
    }

    /// Nodes in `page`.
    async fn page_len(store: &MemoryStore, page: BlobId) -> u64 {
        let bytes = store.get_blob(page).await.unwrap().unwrap();
        Reader::new(&bytes).seq_len().unwrap()
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
        let counting = Counting::new(&store);
        Deriver::new(REPO, &store, &counting)
            .derive::<PathHistory>(root)
            .await
            .unwrap();
        assert!(
            counting.blob_puts() > 1,
            "a large change spans several pages"
        );

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
        assert_eq!(counting.blob_puts(), 1, "one page of history nodes");
        assert_eq!(
            page_len(&store, h.0.page).await,
            DEPTH as u64 + 1,
            "history nodes rewritten"
        );
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
