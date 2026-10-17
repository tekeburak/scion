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
//!
//! Those reads need not wait on each other: an entry also counts the changes
//! before it on its chain (its height) and names the node holding the change
//! at the last multiple of [`ANCHOR_STEP`] below it. [`HistoryEntry::chain`]
//! follows the anchors and walks the segments between them concurrently, so
//! `n` changes cost about `n / ANCHOR_STEP + ANCHOR_STEP` reads in sequence.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt, stream};
use scion_store::ObjectStore;
use scion_types::canonical::{CanonicalError, Reader, Writer};
use scion_types::{
    BlobId, Changeset, ChangesetId, PathComponent, RepoPath, Tree, TreeEntry, TreeId,
};

use crate::root_tree::WRITE_CONCURRENCY;
use crate::{Derivable, DeriveError, Deriver, Result, RootTree};

/// A page is closed once its nodes' encodings reach this many bytes. Small
/// enough that a page stays in the object store's memo of small blobs.
const PAGE_BYTES: usize = 16 << 10;

/// Heights at multiples of this are anchors: every entry names the node
/// holding the newest anchor below it on its chain.
pub const ANCHOR_STEP: u64 = 16;

/// Segments of a chain walked at once by [`HistoryEntry::chain`].
const SEGMENTS: usize = 32;

/// A stored history node: the page (blob) holding it and its index there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeAt {
    pub page: BlobId,
    pub index: u32,
}

/// The root history node of a changeset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathHistory(pub NodeAt);

/// The change that last touched a tree entry, and where its chain goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    /// The most recent first-parent ancestor that changed the entry.
    pub last_changed: ChangesetId,
    /// The history node of the entry's directory at
    /// `first_parent(last_changed)`, the node `last_changed` rewrote; `None`
    /// when the directory did not exist there.
    pub replaced: Option<NodeAt>,
    /// Changes to the entry before this one: 0 for the change that created it.
    pub height: u64,
    /// The node of the entry's directory holding the change at
    /// [`Change::anchor_height`]; `None` at height 0.
    pub anchor: Option<NodeAt>,
}

impl Change {
    /// `last_changed` changing an entry whose previous change, found in the
    /// node `replaced`, was `before`.
    fn after(
        last_changed: ChangesetId,
        replaced: Option<NodeAt>,
        before: Option<&HistoryEntry>,
    ) -> Self {
        let Some(before) = before.map(HistoryEntry::change) else {
            return Self {
                last_changed,
                replaced,
                height: 0,
                anchor: None,
            };
        };
        Self {
            last_changed,
            replaced,
            height: before.height + 1,
            anchor: if before.height % ANCHOR_STEP == 0 {
                replaced
            } else {
                before.anchor
            },
        }
    }

    /// The height of the change `anchor` holds: the largest multiple of
    /// [`ANCHOR_STEP`] below this one's.
    #[must_use]
    pub fn anchor_height(&self) -> Option<u64> {
        (self.height > 0).then(|| (self.height - 1) / ANCHOR_STEP * ANCHOR_STEP)
    }
}

/// History of one tree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryEntry {
    File(Change),
    Dir { change: Change, node: NodeAt },
}

impl HistoryEntry {
    /// The entry's last change.
    #[must_use]
    pub fn change(&self) -> &Change {
        match self {
            Self::File(change) | Self::Dir { change, .. } => change,
        }
    }

    /// The most recent first-parent ancestor that changed this entry.
    #[must_use]
    pub fn last_changed(&self) -> ChangesetId {
        self.change().last_changed
    }

    /// The node this entry's last change replaced.
    #[must_use]
    pub fn replaced(&self) -> Option<NodeAt> {
        self.change().replaced
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
        HistoryNode::entry(objects, at, name).await
    }

    /// The entry `name` has at its anchor, checked to be at the anchor's
    /// height; `None` at height 0. One node read.
    async fn anchored(
        &self,
        objects: &dyn ObjectStore,
        name: &PathComponent,
    ) -> Result<Option<Self>> {
        let (Some(at), Some(height)) = (self.change().anchor, self.change().anchor_height()) else {
            return Ok(None);
        };
        match HistoryNode::entry(objects, at, name).await? {
            Some(entry) if entry.change().height == height => Ok(Some(entry)),
            _ => Err(DeriveError::CorruptObject(at.page)),
        }
    }

    /// This entry and the earlier changes to it, newest first: the entries
    /// at heights `self.height` down to `lowest`, each `previous` of the one
    /// before. The anchors are read one after another, and the segment below
    /// each is walked as soon as the anchor is known.
    pub fn chain<'a>(
        self,
        objects: &'a dyn ObjectStore,
        name: &'a PathComponent,
        lowest: u64,
    ) -> impl Stream<Item = Result<Self>> + Send + 'a {
        let starts = stream::try_unfold(Some(self), move |start| async move {
            let Some(start) = start else {
                return Ok(None);
            };
            let next = match start.change().anchor_height() {
                Some(height) if height >= lowest => start.anchored(objects, name).await?,
                _ => None,
            };
            Ok(Some((start, next)))
        });
        starts
            .map_ok(move |start| start.segment(objects, name, lowest))
            .try_buffered(SEGMENTS)
            .map_ok(|segment| stream::iter(segment.into_iter().map(Ok)))
            .try_flatten()
    }

    /// This entry and the ones before it down to its anchor (exclusive) or
    /// `lowest` (inclusive), newest first.
    async fn segment(
        self,
        objects: &dyn ObjectStore,
        name: &PathComponent,
        lowest: u64,
    ) -> Result<Vec<Self>> {
        let stop = self
            .change()
            .anchor_height()
            .map_or(0, |height| height + 1)
            .max(lowest);
        let mut out = vec![self];
        let mut entry = self;
        while entry.change().height > stop {
            match entry.previous(objects, name).await? {
                Some(before) if before.change().height + 1 == entry.change().height => {
                    out.push(before);
                    entry = before;
                }
                _ => {
                    return Err(DeriveError::CorruptPointer {
                        ty: PathHistory::NAME,
                        cs: entry.last_changed(),
                    });
                }
            }
        }
        Ok(out)
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
    Here { change: Change, index: u32 },
}

type PageNode = BTreeMap<PathComponent, PageEntry>;

const ENTRY_FILE: u8 = 0;
const ENTRY_DIR: u8 = 1;
const ENTRY_DIR_HERE: u8 = 2;

fn write_node_at(w: &mut Writer, at: &NodeAt) {
    w.id(at.page.as_bytes());
    w.u64(u64::from(at.index));
}

fn read_node_at(r: &mut Reader<'_>) -> std::result::Result<NodeAt, CanonicalError> {
    Ok(NodeAt {
        page: BlobId::from_bytes(r.id()?),
        index: u32::try_from(r.u64()?).map_err(|_| CanonicalError::LengthOverflow)?,
    })
}

fn write_change(w: &mut Writer, change: &Change) {
    w.id(change.last_changed.as_bytes());
    w.opt(change.replaced.as_ref(), write_node_at);
    w.u64(change.height);
    w.opt(change.anchor.as_ref(), write_node_at);
}

fn read_change(r: &mut Reader<'_>) -> Option<Change> {
    Some(Change {
        last_changed: ChangesetId::from_bytes(r.id().ok()?),
        replaced: r.opt(read_node_at).ok()?,
        height: r.u64().ok()?,
        anchor: r.opt(read_node_at).ok()?,
    })
}

fn encode_page(nodes: &[PageNode]) -> Vec<u8> {
    let mut w = Writer::new();
    w.seq(nodes.iter(), |w, node| {
        w.seq(node.iter(), |w, (name, entry)| {
            w.str(name.as_str());
            let (tag, change) = match entry {
                PageEntry::Stored(HistoryEntry::File(change)) => (ENTRY_FILE, change),
                PageEntry::Stored(HistoryEntry::Dir { change, .. }) => (ENTRY_DIR, change),
                PageEntry::Here { change, .. } => (ENTRY_DIR_HERE, change),
            };
            w.u8(tag);
            write_change(w, change);
            match entry {
                PageEntry::Stored(HistoryEntry::File(_)) => {}
                PageEntry::Stored(HistoryEntry::Dir { node, .. }) => write_node_at(w, node),
                PageEntry::Here { index, .. } => w.u64(u64::from(*index)),
            }
        });
    });
    w.into_vec()
}

/// Calls `f` with the name and entry of each entry of node `index` of the
/// page `page` whose bytes are `bytes`, until `f` returns `true`; `None` if
/// the page is malformed up to there or has no such node. Nodes before the
/// wanted one are skipped without allocating, since a lookup reads only one
/// entry of a node late in its page.
fn walk_node(
    bytes: &[u8],
    page: BlobId,
    index: u32,
    mut f: impl FnMut(&[u8], HistoryEntry) -> bool,
) -> Option<()> {
    let mut r = Reader::new(bytes);
    let n = r.seq_len().ok()?;
    for i in 0..n {
        let wanted = i == u64::from(index);
        for _ in 0..r.seq_len().ok()? {
            let name = r.bytes().ok()?;
            let tag = r.u8().ok()?;
            let change = read_change(&mut r)?;
            let entry = match tag {
                ENTRY_FILE => HistoryEntry::File(change),
                ENTRY_DIR => HistoryEntry::Dir {
                    change,
                    node: read_node_at(&mut r).ok()?,
                },
                ENTRY_DIR_HERE => {
                    let index = u32::try_from(r.u64().ok()?).ok()?;
                    // Children come before their parent in a page.
                    if u64::from(index) >= i {
                        return None;
                    }
                    HistoryEntry::Dir {
                        change,
                        node: NodeAt { page, index },
                    }
                }
                _ => return None,
            };
            if wanted && f(name, entry) {
                return Some(());
            }
        }
        // The page's bytes were checked against its id when read, so the
        // nodes after the wanted one need not be parsed.
        if wanted {
            return Some(());
        }
    }
    None
}

/// Node `index` of the page `page` whose bytes are `bytes`; `None` if the
/// page is malformed up to that node or has no such node.
fn decode_node(bytes: &[u8], page: BlobId, index: u32) -> Option<HistoryNode> {
    let mut node = HistoryNode::default();
    let mut valid = true;
    walk_node(bytes, page, index, |name, entry| {
        let Some(name) = std::str::from_utf8(name)
            .ok()
            .and_then(|n| PathComponent::new(n).ok())
        else {
            valid = false;
            return true;
        };
        node.entries.insert(name, entry);
        false
    })?;
    valid.then_some(node)
}

impl HistoryNode {
    async fn page(objects: &dyn ObjectStore, at: NodeAt) -> Result<Bytes> {
        objects
            .get_blob(at.page)
            .await?
            .ok_or(DeriveError::MissingObject(at.page))
    }

    /// The entry `name` of the node at `at`, without decoding the rest.
    async fn entry(
        objects: &dyn ObjectStore,
        at: NodeAt,
        name: &PathComponent,
    ) -> Result<Option<HistoryEntry>> {
        let bytes = Self::page(objects, at).await?;
        let mut found = None;
        walk_node(&bytes, at.page, at.index, |n, entry| {
            let hit = n == name.as_str().as_bytes();
            if hit {
                found = Some(entry);
            }
            hit
        })
        .ok_or(DeriveError::CorruptObject(at.page))?;
        Ok(found)
    }

    async fn load(objects: &dyn ObjectStore, at: NodeAt) -> Result<Self> {
        let bytes = Self::page(objects, at).await?;
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
    change: Change,
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
        for NewEntry { name, change, dir } in entries {
            self.open_bytes += name.as_str().len() + 153;
            let entry = match dir {
                None => PageEntry::Stored(HistoryEntry::File(change)),
                Some(Built::Stored(at)) => {
                    PageEntry::Stored(HistoryEntry::Dir { change, node: at })
                }
                Some(Built::New { page: p, index }) if p == page => {
                    PageEntry::Here { change, index }
                }
                Some(Built::New { page: p, index }) => PageEntry::Stored(HistoryEntry::Dir {
                    change,
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
    const VERSION: u32 = 4;

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
                HistoryEntry::File(_) => None,
                HistoryEntry::Dir { node, .. } => Some(Built::Stored(*node)),
            };
            entries.push(NewEntry {
                name: name.clone(),
                change: *unchanged.change(),
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
            change: Change::after(
                cs,
                old.map(|(_, history)| history),
                old_history.entries.get(name),
            ),
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
        let mut at = self.0;
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            let entry = HistoryNode::entry(objects, at, &component).await?;
            match entry {
                Some(HistoryEntry::Dir { node, .. }) if components.peek().is_some() => at = node,
                _ if components.peek().is_some() => return Ok(None),
                entry => return Ok(entry),
            }
        }
        unreachable!("paths are non-empty")
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
        let first = Change {
            last_changed: a,
            replaced: None,
            height: 0,
            anchor: None,
        };
        let later = Change {
            last_changed: b,
            replaced: before,
            height: 20,
            anchor: Some(elsewhere),
        };
        let file = HistoryEntry::File(first);
        let child = PageNode::from([(name("f"), PageEntry::Stored(file))]);
        let parent = PageNode::from([
            (
                name("d"),
                PageEntry::Here {
                    change: later,
                    index: 0,
                },
            ),
            (
                name("e"),
                PageEntry::Stored(HistoryEntry::Dir {
                    change: later,
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
                change: later,
                node: NodeAt { page, index: 0 },
            },
            "a child in the same page is named by the page being read"
        );
        assert_eq!(
            parent.entries[&name("e")],
            HistoryEntry::Dir {
                change: later,
                node: elsewhere,
            }
        );
        assert_eq!(decode_node(&bytes, page, 2), None, "no such node");
        assert_eq!(decode_node(b"garbage", page, 0), None);
        let forward = encode_page(&[PageNode::from([(
            name("d"),
            PageEntry::Here {
                change: first,
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
