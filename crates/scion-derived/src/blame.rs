//! `Blame`: for every line of a file, the changeset that introduced it.
//!
//! A changeset's `Blame` value holds the line attribution of **only the files
//! that changeset changed**, so deriving it costs the size of the change, and
//! the attribution of a file at any changeset is found through
//! [`PathHistory`]: `blame(cs, path)` is the entry for `path` in
//! `Blame(last_changed)` where `last_changed = PathHistory(cs).lookup(path)`.
//!
//! Each changed file is attributed by diffing it (with `diffy`) against its
//! previous version: the file at the same path in the first parent, or — when
//! the change carries `copy_from` — the named source path at the named
//! changeset, which is how renames keep their history. Unchanged lines keep
//! their attribution; inserted lines belong to the changeset.
//!
//! Lines are byte strings split on `\n`; a final line without a newline
//! counts as a line, exactly as `diffy` counts them.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bytes::Bytes;
use diffy::{DiffOptions, Line};
use futures_util::{StreamExt, TryStreamExt, stream};
use scion_store::ObjectStore;
use scion_types::canonical::{Reader, Writer};
use scion_types::{BlobId, Changeset, ChangesetId, FileChange, RepoPath, TreeEntry};

use crate::{Derivable, DeriveError, Deriver, HistoryEntry, PathHistory, Result};

/// The blame node of a changeset: attribution of the files it changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blame(pub BlobId);

/// Per-line attribution of one file, run-length encoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileBlame {
    /// `(changeset, number of consecutive lines)`.
    pub runs: Vec<(ChangesetId, u64)>,
}

impl FileBlame {
    /// Every line attributed to `cs`.
    #[must_use]
    pub fn uniform(cs: ChangesetId, lines: u64) -> Self {
        let runs = if lines == 0 {
            vec![]
        } else {
            vec![(cs, lines)]
        };
        Self { runs }
    }

    /// Number of lines.
    #[must_use]
    pub fn line_count(&self) -> u64 {
        self.runs.iter().map(|(_, n)| n).sum()
    }

    /// The changeset of each line, in order.
    pub fn lines(&self) -> impl Iterator<Item = ChangesetId> + '_ {
        self.runs
            .iter()
            .flat_map(|(cs, n)| std::iter::repeat_n(*cs, usize::try_from(*n).unwrap_or(usize::MAX)))
    }

    fn push(&mut self, cs: ChangesetId) {
        match self.runs.last_mut() {
            Some((last, n)) if *last == cs => *n += 1,
            _ => self.runs.push((cs, 1)),
        }
    }
}

/// `path -> FileBlame` for the files a changeset changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlameNode {
    pub files: BTreeMap<RepoPath, FileBlame>,
}

impl BlameNode {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.seq(self.files.iter(), |w, (path, file)| {
            w.str(path.as_str());
            w.seq(file.runs.iter(), |w, (cs, n)| {
                w.id(cs.as_bytes());
                w.u64(*n);
            });
        });
        w.into_vec()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader::new(bytes);
        let n_files = r.seq_len().ok()?;
        let mut files = BTreeMap::new();
        for _ in 0..n_files {
            let path = RepoPath::new(r.str().ok()?).ok()?;
            let n_runs = r.seq_len().ok()?;
            let mut runs = Vec::new();
            for _ in 0..n_runs {
                let cs = ChangesetId::from_bytes(r.id().ok()?);
                let count = r.u64().ok()?;
                runs.push((cs, count));
            }
            files.insert(path, FileBlame { runs });
        }
        r.finish().ok()?;
        Some(Self { files })
    }

    async fn load(objects: &dyn ObjectStore, id: BlobId) -> Result<Self> {
        let bytes = objects
            .get_blob(id)
            .await?
            .ok_or(DeriveError::MissingObject(id))?;
        Self::decode(&bytes).ok_or(DeriveError::CorruptObject(id))
    }
}

/// Number of lines in `bytes`, the way `diffy` counts them.
fn line_count(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.split_inclusive(|b| *b == b'\n').count()).expect("fits")
}

/// Attribute `new` given `old` and its attribution: unchanged lines keep
/// theirs, inserted lines belong to `cs`.
fn attribute(old: &[u8], old_blame: &FileBlame, new: &[u8], cs: ChangesetId) -> Option<FileBlame> {
    // One hunk spanning the whole file, so the line sequence is complete.
    let patch = DiffOptions::new()
        .set_context_len(usize::MAX)
        .create_patch_bytes(old, new);
    if patch.hunks().is_empty() {
        return Some(old_blame.clone());
    }
    let mut old_lines = old_blame.lines();
    let mut out = FileBlame::default();
    for hunk in patch.hunks() {
        for line in hunk.lines() {
            match line {
                Line::Context(_) => out.push(old_lines.next()?),
                Line::Delete(_) => {
                    old_lines.next()?;
                }
                Line::Insert(_) => out.push(cs),
            }
        }
    }
    Some(out)
}

/// Versions read ahead of the one being attributed, and files of one
/// changeset attributed at once.
const READ_AHEAD: usize = 16;

/// One version of a file on its chain: the changeset that wrote it and the
/// content it wrote.
#[derive(Debug, Clone, Copy)]
struct Version {
    cs: ChangesetId,
    content: BlobId,
}

/// The history entry of the file a changed `path` continues, and its path
/// there: the `copy_from` source, else `path` in the first parent.
async fn version_before(
    ctx: &Deriver<'_>,
    path: &RepoPath,
    copy_from: Option<&(RepoPath, ChangesetId)>,
    first_parent: Option<ChangesetId>,
) -> Result<Option<(HistoryEntry, RepoPath)>> {
    let (at, from) = match (copy_from, first_parent) {
        (Some((from, at)), _) => (*at, from),
        (None, Some(parent)) => (parent, path),
        (None, None) => return Ok(None),
    };
    let history = ctx.derive::<PathHistory>(at).await?;
    Ok(file_entry(history.lookup(ctx.objects, from).await?).map(|e| (e, from.clone())))
}

fn file_entry(entry: Option<HistoryEntry>) -> Option<HistoryEntry> {
    entry.filter(|e| matches!(e, HistoryEntry::File { .. }))
}

async fn get_blob(objects: &dyn ObjectStore, id: BlobId) -> Result<Bytes> {
    objects
        .get_blob(id)
        .await?
        .ok_or(DeriveError::MissingObject(id))
}

/// The attribution of `path` as changed by `cs` (to `entry`, from
/// `copy_from`), or `None` when the change leaves the file's entry as it was.
///
/// Walks the file's own version chain back to a version whose changeset's
/// blame node is stored, or to the file's creation, then attributes forward
/// along it. The versions in between get no node of their own: a node holds
/// every file its changeset changed, and deriving those would pull in their
/// chains too.
async fn file_blame(
    ctx: &Deriver<'_>,
    cs: &Changeset,
    path: &RepoPath,
    entry: TreeEntry,
    copy_from: Option<&(RepoPath, ChangesetId)>,
) -> Result<Option<FileBlame>> {
    let TreeEntry::Blob { id: content, .. } = entry else {
        unreachable!("a tracked change is a file")
    };
    // Newest first; `base` is the stored attribution the chain ends on.
    let mut chain = vec![Version {
        cs: cs.id(),
        content,
    }];
    let mut base = None;
    let first_copy_from = copy_from;
    let mut before = version_before(ctx, path, copy_from, cs.first_parent()).await?;
    while let Some((history, from)) = before {
        let at = history.last_changed();
        // The version before this one is `from` in the node `at` replaced,
        // unless `at` copied the file: read it while `at` is loaded, so a
        // hop costs one round trip.
        let name = from.file_name();
        let (changeset, stored, replaced) = futures_util::try_join!(
            async {
                ctx.meta
                    .get_changeset(ctx.repo, at)
                    .await?
                    .ok_or(DeriveError::MissingChangeset(at))
            },
            ctx.fetch::<Blame>(at),
            history.previous(ctx.objects, &name),
        )?;
        let Some(FileChange::Tracked {
            content,
            kind,
            size,
            copy_from,
        }) = changeset.file_changes.get(&from)
        else {
            return Err(DeriveError::CorruptPointer {
                ty: PathHistory::NAME,
                cs: at,
            });
        };
        // Same entry at the same path: the tree did not change here, so
        // PathHistory keeps pointing at the older changeset and this node
        // does not hold the file.
        let old_entry = TreeEntry::Blob {
            id: *content,
            kind: *kind,
            size: *size,
        };
        if chain.len() == 1 && first_copy_from.is_none() && old_entry == entry {
            return Ok(None);
        }
        let version = Version {
            cs: at,
            content: *content,
        };
        if let Some(Blame(node)) = stored {
            base = Some((version, from, node));
            break;
        }
        chain.push(version);
        before = match copy_from {
            Some(_) => version_before(ctx, &from, copy_from.as_ref(), None).await?,
            None => file_entry(replaced).map(|e| (e, from)),
        };
    }

    // Attribution only fails on a stored node that does not match its file.
    let corrupt = DeriveError::CorruptObject(base.as_ref().map_or(content, |(.., node)| *node));
    let mut current: Option<(FileBlame, Bytes)> = match base {
        None => None,
        Some((version, from, node)) => {
            let (stored, bytes) = futures_util::try_join!(
                BlameNode::load(ctx.objects, node),
                get_blob(ctx.objects, version.content),
            )?;
            let file = stored
                .files
                .get(&from)
                .cloned()
                .ok_or(DeriveError::CorruptObject(node))?;
            Some((file, bytes))
        }
    };
    let ids: Vec<BlobId> = chain.iter().rev().map(|v| v.content).collect();
    let mut contents = stream::iter(ids)
        .map(|id| get_blob(ctx.objects, id))
        .buffered(READ_AHEAD);
    let mut versions = chain.iter().rev();
    while let Some(new) = contents.try_next().await? {
        let version = versions.next().expect("one content per version");
        let file = match &current {
            None => FileBlame::uniform(version.cs, line_count(&new)),
            Some((old_blame, old)) if *old == new => old_blame.clone(),
            Some((old_blame, old)) => match attribute(old, old_blame, &new, version.cs) {
                Some(file) => file,
                None => return Err(corrupt),
            },
        };
        current = Some((file, new));
    }
    Ok(current.map(|(file, _)| file))
}

#[async_trait]
impl Derivable for Blame {
    const NAME: &'static str = "blame";
    const VERSION: u32 = 1;
    // A node holds only the files its changeset changed, attributed along
    // each file's own version chain: the parents' nodes are not what it is
    // built from.
    const READS_PARENTS: bool = false;

    async fn derive(ctx: &Deriver<'_>, cs: &Changeset, _parents: &[Self]) -> Result<Self> {
        let changed: Vec<_> = cs
            .file_changes
            .iter()
            .filter_map(|(path, change)| match change {
                FileChange::Tracked {
                    content,
                    kind,
                    size,
                    copy_from,
                } => Some((
                    path.clone(),
                    TreeEntry::Blob {
                        id: *content,
                        kind: *kind,
                        size: *size,
                    },
                    copy_from.clone(),
                )),
                FileChange::Deleted => None,
            })
            .collect();
        let files: Vec<_> = stream::iter(changed)
            .map(|(path, entry, copy_from)| async move {
                let file = file_blame(ctx, cs, &path, entry, copy_from.as_ref()).await?;
                Ok::<_, DeriveError>(file.map(|f| (path, f)))
            })
            .buffered(READ_AHEAD)
            .try_collect()
            .await?;
        let node = BlameNode {
            files: files.into_iter().flatten().collect(),
        };
        Ok(Self(
            ctx.objects.put_blob(Bytes::from(node.encode())).await?,
        ))
    }

    fn to_pointer(&self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }

    fn from_pointer(bytes: &[u8]) -> Option<Self> {
        BlobId::try_from_slice(bytes).ok().map(Self)
    }
}

impl Blame {
    /// The attribution of `path` at `cs`, or `None` if there is no file there.
    pub async fn for_path(
        ctx: &Deriver<'_>,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<FileBlame>> {
        let history = ctx.derive::<PathHistory>(cs).await?;
        let Some(HistoryEntry::File { last_changed, .. }) =
            history.lookup(ctx.objects, path).await?
        else {
            return Ok(None);
        };
        let blame = ctx.derive::<Blame>(last_changed).await?;
        let node = BlameNode::load(ctx.objects, blame.0).await?;
        node.files
            .get(path)
            .cloned()
            .map(Some)
            .ok_or(DeriveError::CorruptObject(blame.0))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use scion_store::{MemoryStore, MetadataStore};
    use scion_types::FileType;

    use super::*;
    use crate::test_util::{Counting, CountingMeta, REPO, changeset};

    // ---- naive reference: lines carry the changeset that wrote them ------

    /// A file as a list of unique lines, each tagged with its author.
    #[derive(Clone, Default, Debug)]
    struct Model {
        lines: Vec<(u32, ChangesetId)>,
    }

    impl Model {
        fn content(&self, trailing_newline: bool) -> Vec<u8> {
            let body = self
                .lines
                .iter()
                .map(|(token, _)| format!("line-{token}"))
                .collect::<Vec<_>>()
                .join("\n");
            let mut bytes = body.into_bytes();
            if trailing_newline && !bytes.is_empty() {
                bytes.push(b'\n');
            }
            bytes
        }
        fn tags(&self) -> Vec<ChangesetId> {
            self.lines.iter().map(|(_, cs)| *cs).collect()
        }
    }

    /// Store `bytes` as a blob and describe it as a tracked change.
    async fn tracked_bytes(
        store: &MemoryStore,
        bytes: &[u8],
        kind: FileType,
        copy_from: Option<(RepoPath, ChangesetId)>,
    ) -> FileChange {
        let content = store.put_blob(Bytes::copy_from_slice(bytes)).await.unwrap();
        FileChange::Tracked {
            content,
            kind,
            size: u64::try_from(bytes.len()).unwrap(),
            copy_from,
        }
    }

    async fn blame_lines(d: &Deriver<'_>, cs: ChangesetId, path: &str) -> Option<Vec<ChangesetId>> {
        Blame::for_path(d, cs, &RepoPath::new(path).unwrap())
            .await
            .unwrap()
            .map(|b| b.lines().collect())
    }

    // ---- tests ----------------------------------------------------------

    #[test]
    fn line_count_matches_diffy_convention() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"a"), 1);
        assert_eq!(line_count(b"a\n"), 1);
        assert_eq!(line_count(b"a\nb"), 2);
        assert_eq!(line_count(b"a\nb\n"), 2);
        assert_eq!(line_count(b"\n\n"), 2);
    }

    #[test]
    fn node_encoding_roundtrips() {
        let mut node = BlameNode::default();
        node.files.insert(
            RepoPath::new("a/b").unwrap(),
            FileBlame {
                runs: vec![
                    (ChangesetId::from_bytes([1; 32]), 3),
                    (ChangesetId::from_bytes([2; 32]), 1),
                ],
            },
        );
        node.files
            .insert(RepoPath::new("empty").unwrap(), FileBlame::default());
        assert_eq!(BlameNode::decode(&node.encode()), Some(node));
        assert_eq!(BlameNode::decode(b"nope"), None);
    }

    #[tokio::test]
    async fn new_edit_rename_and_readd() {
        let store = MemoryStore::new();
        let d = Deriver::new(REPO, &store, &store);
        let path = |p: &str| RepoPath::new(p).unwrap();

        // c0: new file, three lines.
        let mut ch = BTreeMap::new();
        ch.insert(
            path("f"),
            tracked_bytes(&store, b"a\nb\nc\n", FileType::Regular, None).await,
        );
        let c0 = store
            .put_changeset(&changeset(vec![], ch, "c0"))
            .await
            .unwrap();
        assert_eq!(blame_lines(&d, c0, "f").await, Some(vec![c0; 3]));

        // c1: replace the middle line, append one.
        let mut ch = BTreeMap::new();
        ch.insert(
            path("f"),
            tracked_bytes(&store, b"a\nB\nc\nd\n", FileType::Regular, None).await,
        );
        let c1 = store
            .put_changeset(&changeset(vec![c0], ch, "c1"))
            .await
            .unwrap();
        assert_eq!(blame_lines(&d, c1, "f").await, Some(vec![c0, c1, c0, c1]));

        // c2: mode change only; content identical → attribution unchanged,
        // and PathHistory points at c2, so Blame(c2) must carry the file.
        let mut ch = BTreeMap::new();
        ch.insert(
            path("f"),
            tracked_bytes(&store, b"a\nB\nc\nd\n", FileType::Executable, None).await,
        );
        let c2 = store
            .put_changeset(&changeset(vec![c1], ch, "c2"))
            .await
            .unwrap();
        assert_eq!(blame_lines(&d, c2, "f").await, Some(vec![c0, c1, c0, c1]));

        // c3: rename f → g with copy_from, dropping the last line.
        let mut ch = BTreeMap::new();
        ch.insert(path("f"), FileChange::Deleted);
        ch.insert(
            path("g"),
            tracked_bytes(
                &store,
                b"a\nB\nc\n",
                FileType::Executable,
                Some((path("f"), c2)),
            )
            .await,
        );
        let c3 = store
            .put_changeset(&changeset(vec![c2], ch, "c3"))
            .await
            .unwrap();
        assert_eq!(blame_lines(&d, c3, "g").await, Some(vec![c0, c1, c0]));
        assert_eq!(blame_lines(&d, c3, "f").await, None);

        // c4: delete g; c5: re-add g with the same content → fresh attribution.
        let mut ch = BTreeMap::new();
        ch.insert(path("g"), FileChange::Deleted);
        let c4 = store
            .put_changeset(&changeset(vec![c3], ch, "c4"))
            .await
            .unwrap();
        let mut ch = BTreeMap::new();
        ch.insert(
            path("g"),
            tracked_bytes(&store, b"a\nB\nc\n", FileType::Regular, None).await,
        );
        let c5 = store
            .put_changeset(&changeset(vec![c4], ch, "c5"))
            .await
            .unwrap();
        assert_eq!(blame_lines(&d, c4, "g").await, None);
        assert_eq!(blame_lines(&d, c5, "g").await, Some(vec![c5; 3]));

        // A directory and a missing path have no blame.
        assert_eq!(blame_lines(&d, c5, "nope").await, None);
    }

    #[tokio::test]
    async fn non_utf8_and_empty_files() {
        let store = MemoryStore::new();
        let d = Deriver::new(REPO, &store, &store);
        let mut ch = BTreeMap::new();
        ch.insert(
            RepoPath::new("bin").unwrap(),
            tracked_bytes(&store, b"\xff\xfe\n\x00\x01", FileType::Regular, None).await,
        );
        ch.insert(
            RepoPath::new("empty").unwrap(),
            tracked_bytes(&store, b"", FileType::Regular, None).await,
        );
        let c0 = store
            .put_changeset(&changeset(vec![], ch, "c0"))
            .await
            .unwrap();
        assert_eq!(blame_lines(&d, c0, "bin").await, Some(vec![c0; 2]));
        assert_eq!(blame_lines(&d, c0, "empty").await, Some(vec![]));

        let mut ch = BTreeMap::new();
        ch.insert(
            RepoPath::new("bin").unwrap(),
            tracked_bytes(&store, b"\xff\xfe\n\x00\x02", FileType::Regular, None).await,
        );
        let c1 = store
            .put_changeset(&changeset(vec![c0], ch, "c1"))
            .await
            .unwrap();
        assert_eq!(blame_lines(&d, c1, "bin").await, Some(vec![c0, c1]));
    }

    #[tokio::test]
    async fn blame_node_holds_only_changed_files() {
        let store = MemoryStore::new();
        let mut ch = BTreeMap::new();
        for i in 0..200 {
            ch.insert(
                RepoPath::new(format!("d{}/f{i}", i % 10)).unwrap(),
                tracked_bytes(&store, format!("{i}\n").as_bytes(), FileType::Regular, None).await,
            );
        }
        let c0 = store
            .put_changeset(&changeset(vec![], ch, "c0"))
            .await
            .unwrap();
        // c0's own derived data is in place before we start counting.
        let d0 = Deriver::new(REPO, &store, &store);
        d0.derive::<Blame>(c0).await.unwrap();
        d0.derive::<PathHistory>(c0).await.unwrap();

        let mut ch = BTreeMap::new();
        ch.insert(
            RepoPath::new("d3/f3").unwrap(),
            tracked_bytes(&store, b"3\nmore\n", FileType::Regular, None).await,
        );
        let c1 = store
            .put_changeset(&changeset(vec![c0], ch, "c1"))
            .await
            .unwrap();
        let counting = Counting::new(&store);
        let d = Deriver::new(REPO, &store, &counting);
        let Blame(node) = d.derive::<Blame>(c1).await.unwrap();
        let node = BlameNode::load(&store, node).await.unwrap();
        assert_eq!(node.files.len(), 1);
        // The previous version is read through c0's data; the only write is
        // c1's blame node.
        assert_eq!(counting.blob_puts(), 1);
        assert_eq!(counting.tree_puts(), 0);
    }

    #[tokio::test]
    async fn cold_blame_reads_the_file_versions_not_the_history() {
        const N: usize = 900;
        const EVERY: usize = 3;
        let store = MemoryStore::new();
        let hot = RepoPath::new("src/hot.rs").unwrap();
        let mut ids: Vec<ChangesetId> = Vec::new();
        let mut versions = Vec::new();
        let mut content = String::new();
        for i in 0..N {
            let mut ch = BTreeMap::new();
            if i % EVERY == 0 {
                content += &(i.to_string() + "\n");
                ch.insert(
                    hot.clone(),
                    tracked_bytes(&store, content.as_bytes(), FileType::Regular, None).await,
                );
            } else {
                ch.insert(
                    RepoPath::new(format!("other/f{i}")).unwrap(),
                    tracked_bytes(&store, b"x\n", FileType::Regular, None).await,
                );
            }
            let id = store
                .put_changeset(&changeset(
                    ids.last().copied().into_iter().collect(),
                    ch,
                    &i.to_string(),
                ))
                .await
                .unwrap();
            if i % EVERY == 0 {
                versions.push(id);
            }
            ids.push(id);
        }
        // What CreateChangeset derives eagerly; blame is cold.
        Deriver::new(REPO, &store, &store)
            .derive::<PathHistory>(ids[N - 1])
            .await
            .unwrap();

        let meta = CountingMeta::new(&store);
        let d = Deriver::new(REPO, &meta, &store);
        let got = Blame::for_path(&d, ids[N - 1], &hot)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.lines().collect::<Vec<_>>(), versions);
        let k = versions.len();
        assert!(
            meta.changeset_reads() <= k + 1,
            "changesets read: {} for {k} versions in {N} changesets",
            meta.changeset_reads()
        );
        assert!(
            meta.derived_reads() <= 2 * k + 2,
            "pointers read: {}",
            meta.derived_reads()
        );
        // The head's node is stored: blaming it again derives nothing.
        let meta = CountingMeta::new(&store);
        let d = Deriver::new(REPO, &meta, &store);
        assert_eq!(
            Blame::for_path(&d, ids[N - 1], &hot)
                .await
                .unwrap()
                .unwrap(),
            got
        );
        assert_eq!(meta.changeset_reads(), 0);
        // An older version walks its own, shorter chain.
        let at = versions[k / 2];
        let mid = Blame::for_path(&d, at, &hot).await.unwrap().unwrap();
        assert_eq!(mid.lines().collect::<Vec<_>>(), versions[..=k / 2]);
    }

    // ---- property test: diff attribution ≡ tagged-line model ------------

    #[derive(Debug, Clone)]
    struct Step {
        deletes: Vec<prop::sample::Index>,
        inserts: Vec<prop::sample::Index>,
        trailing_newline: bool,
        rename: bool,
    }

    fn arb_step() -> impl Strategy<Value = Step> {
        (
            prop::collection::vec(any::<prop::sample::Index>(), 0..3),
            prop::collection::vec(any::<prop::sample::Index>(), 0..4),
            any::<bool>(),
            prop::bool::weighted(0.2),
        )
            .prop_map(|(deletes, inserts, trailing_newline, rename)| Step {
                deletes,
                inserts,
                trailing_newline,
                rename,
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn attribution_equals_model(steps in prop::collection::vec(arb_step(), 1..8)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let store = MemoryStore::new();
                let d = Deriver::new(REPO, &store, &store);
                let mut model = Model::default();
                let mut next_token = 0u32;
                let mut parent: Option<ChangesetId> = None;
                let mut path = RepoPath::new("f0").unwrap();
                let mut old_trailing = false;

                for (i, step) in steps.iter().enumerate() {
                    // Edits are deletions of existing lines and insertions of
                    // fresh unique lines, so the common lines keep their order
                    // and every correct diff recovers the same matching.
                    let cs = changeset(parent.into_iter().collect(), BTreeMap::new(), &i.to_string());

                    let mut lines = model.lines.clone();
                    for idx in &step.deletes {
                        if !lines.is_empty() {
                            lines.remove(idx.index(lines.len()));
                        }
                    }
                    // Insert positions are resolved after deletions; the author
                    // is filled in once the changeset id is known.
                    let mut insert_at: Vec<usize> = step.inserts.iter().map(|idx| idx.index(lines.len() + 1)).collect();
                    insert_at.sort_unstable();
                    let placeholder = ChangesetId::from_bytes([0; 32]);
                    for (k, pos) in insert_at.iter().enumerate() {
                        lines.insert(pos + k, (next_token, placeholder));
                        next_token += 1;
                    }
                    // A line's identity includes its newline: the last line of
                    // a file without a trailing newline differs from the same
                    // text with one, so such a line counts as rewritten (as in
                    // git).
                    let (old_len, new_len) = (model.lines.len(), lines.len());
                    for (new_idx, line) in lines.iter_mut().enumerate() {
                        if line.1 == placeholder {
                            continue;
                        }
                        let old_idx = model.lines.iter().position(|(t, _)| *t == line.0).unwrap();
                        let old_nl = old_idx + 1 != old_len || old_trailing;
                        let new_nl = new_idx + 1 != new_len || step.trailing_newline;
                        if old_nl != new_nl {
                            line.1 = placeholder;
                        }
                    }
                    let new_model = Model { lines };
                    let content = new_model.content(step.trailing_newline);

                    let mut changes = BTreeMap::new();
                    let new_path = if step.rename && parent.is_some() {
                        let p = RepoPath::new(format!("f{i}")).unwrap();
                        changes.insert(path.clone(), FileChange::Deleted);
                        p
                    } else {
                        path.clone()
                    };
                    let copy_from = (new_path != path).then(|| (path.clone(), parent.unwrap()));
                    changes.insert(new_path.clone(), tracked_bytes(&store, &content, FileType::Regular, copy_from).await);
                    let cs = Changeset { file_changes: changes, ..cs };
                    let id = store.put_changeset(&cs).await.unwrap();

                    model = Model {
                        lines: new_model.lines.into_iter().map(|(t, c)| (t, if c == placeholder { id } else { c })).collect(),
                    };
                    path = new_path;
                    parent = Some(id);
                    old_trailing = step.trailing_newline;

                    let got = Blame::for_path(&d, id, &path).await.unwrap().expect("file exists");
                    prop_assert_eq!(got.line_count(), line_count(&content), "line count at step {}", i);
                    prop_assert_eq!(got.lines().collect::<Vec<_>>(), model.tags(), "attribution at step {}", i);
                }
                Ok(())
            })?;
        }
    }
}
