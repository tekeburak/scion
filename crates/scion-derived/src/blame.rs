//! `Blame`: for every line of a file, the changeset that introduced it.
//!
//! A changeset's `Blame` value holds the line attribution of **only the files
//! that changeset changed**, so deriving it costs the size of the change, and
//! the attribution of a file at any changeset is found through
//! [`PathHistory`]: `blame(cs, path)` is the entry for `path` in
//! `Blame(last_changed)` where `last_changed = PathHistory(cs).lookup(path)`.
//!
//! Each changed file is attributed by diffing it (a histogram line diff)
//! against its previous version: the file at the same path in the first
//! parent, or — when the change carries `copy_from` — the named source path
//! at the named changeset, which is how renames keep their history.
//! Unchanged lines keep their attribution; inserted lines belong to the
//! changeset.
//!
//! Lines are byte strings split on `\n`; a final line without a newline
//! counts as a line, exactly as the diff counts them.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt, future, stream};
use imara_diff::sources::byte_lines;
use imara_diff::{Algorithm, Diff, Hunk, InternedInput};
use scion_store::ObjectStore;
use scion_types::canonical::{Reader, Writer};
use scion_types::{BlobId, Changeset, ChangesetId, FileChange, PathComponent, RepoPath, TreeEntry};

use crate::{Change, Derivable, DeriveError, Deriver, HistoryEntry, PathHistory, Result};

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
    pub(crate) fn encode(&self) -> Vec<u8> {
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

    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
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

/// Number of lines in `bytes`, the way the diff counts them.
fn line_count(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.split_inclusive(|b| *b == b'\n').count()).expect("fits")
}

/// How one version of a file became the next: the changed regions, in
/// order, of a version `lines` lines long.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Script {
    lines: u32,
    hunks: Vec<Hunk>,
}

/// The line diff from `old` to `new`. It is the expensive part of
/// attribution and needs no attribution, so the scripts of a chain are
/// computed concurrently.
fn script(old: &[u8], new: &[u8]) -> Script {
    let input = InternedInput::new(byte_lines(old), byte_lines(new));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    Script {
        lines: u32::try_from(input.before.len()).expect("the diff counts lines in u32"),
        hunks: diff.hunks().collect(),
    }
}

/// Apply `script` to `old_blame`: unchanged lines keep their attribution,
/// inserted lines belong to `cs`. `None` if `old_blame` does not have the
/// script's line count.
fn attribute(old_blame: &FileBlame, script: &Script, cs: ChangesetId) -> Option<FileBlame> {
    if old_blame.line_count() != u64::from(script.lines) {
        return None;
    }
    let old: Vec<ChangesetId> = old_blame.lines().collect();
    let mut out = FileBlame::default();
    let mut kept_from = 0;
    for hunk in &script.hunks {
        for line in &old[kept_from..hunk.before.start as usize] {
            out.push(*line);
        }
        for _ in hunk.after.clone() {
            out.push(cs);
        }
        kept_from = hunk.before.end as usize;
    }
    for line in &old[kept_from..] {
        out.push(*line);
    }
    Some(out)
}

/// [`script`] on the blocking pool, so the diffs of a chain, and of the
/// files one changeset changed, use every core.
async fn script_apart(old: Bytes, new: Bytes) -> Script {
    tokio::task::spawn_blocking(move || script(&old, &new))
        .await
        .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
}

/// Versions read ahead of the one being attributed, and files of one
/// changeset attributed at once.
const READ_AHEAD: usize = 16;

/// Batches of versions read at once.
const BATCHES: usize = 4;

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
    Ok(file_entry(history.lookup(ctx, from).await?).map(|e| (e, from.clone())))
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

/// The changeset and stored blame of each version of the file whose history
/// entry is `history`, newest first, until the entry is not a file. The
/// entries come through the anchors and are read in batches, each batch
/// what the chain has produced so far, ahead of the consumer.
fn versions<'a>(
    ctx: &'a Deriver<'_>,
    history: HistoryEntry,
    name: &'a PathComponent,
) -> impl Stream<Item = Result<(Changeset, Option<Blame>)>> + 'a {
    history
        .chain(ctx, name, 0)
        .try_take_while(|entry| future::ready(Ok(matches!(entry, HistoryEntry::File(_)))))
        .try_ready_chunks(READ_AHEAD)
        .map_err(|e| e.1)
        .map_ok(move |entries| async move {
            let ids: Vec<ChangesetId> = entries.iter().map(HistoryEntry::last_changed).collect();
            let (changesets, stored) = futures_util::try_join!(
                async { Ok(ctx.meta.get_changesets(ctx.repo, &ids).await?) },
                ctx.fetch_many::<Blame>(&ids),
            )?;
            ids.iter()
                .zip(changesets)
                .zip(stored)
                .map(|((id, cs), stored)| {
                    Ok((cs.ok_or(DeriveError::MissingChangeset(*id))?, stored))
                })
                .collect::<Result<Vec<_>>>()
        })
        .try_buffered(BATCHES)
        .map_ok(|batch| stream::iter(batch.into_iter().map(Ok)))
        .try_flatten()
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
    read_ahead: usize,
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
    // The file's history entries at `from` are found through the anchors,
    // and each version's changeset and stored blame are read ahead of the
    // one being looked at; a copy restarts the walk at its source.
    'paths: while let Some((history, from)) = before.take() {
        let name = from.file_name();
        let mut versions = std::pin::pin!(versions(ctx, history, &name));
        while let Some((changeset, stored)) = versions.try_next().await? {
            let at = changeset.id();
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
                break 'paths;
            }
            chain.push(version);
            if copy_from.is_some() {
                before = version_before(ctx, &from, copy_from.as_ref(), None).await?;
                continue 'paths;
            }
        }
    }

    // Attribution only fails on a stored node that does not match its file.
    let corrupt = base.as_ref().map_or(content, |(.., node)| *node);
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
    // Each version is diffed against the one before it as soon as both are
    // read; only applying the scripts, which is cheap, runs in order.
    let ids: Vec<BlobId> = chain.iter().rev().map(|v| v.content).collect();
    let mut previous = current.as_ref().map(|(_, bytes)| bytes.clone());
    let mut scripts = stream::iter(ids)
        .map(|id| get_blob(ctx.objects, id))
        .buffered(read_ahead)
        .map_ok(move |new| {
            let old = previous.replace(new.clone());
            async move {
                let script = match old {
                    Some(old) if old != new => Some(script_apart(old, new.clone()).await),
                    Some(_) => Some(Script::default()),
                    None => None,
                };
                Ok::<_, DeriveError>((new, script))
            }
        })
        .try_buffered(read_ahead);
    let mut versions = chain.iter().rev();
    while let Some((new, script)) = scripts.try_next().await? {
        let version = versions.next().expect("one content per version");
        let file = match (&current, script) {
            (Some((old_blame, _)), Some(script)) if script.hunks.is_empty() => old_blame.clone(),
            (Some((old_blame, _)), Some(script)) => attribute(old_blame, &script, version.cs)
                .ok_or(DeriveError::CorruptObject(corrupt))?,
            _ => FileBlame::uniform(version.cs, line_count(&new)),
        };
        current = Some((file, new));
    }
    Ok(current.map(|(file, _)| file))
}

/// The node of `cs`: each file it changed attributed along its own version
/// chain, `read_ahead` files at once and `read_ahead` versions of each.
async fn node(ctx: &Deriver<'_>, cs: &Changeset, read_ahead: usize) -> Result<BlameNode> {
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
            let file = file_blame(ctx, cs, &path, entry, copy_from.as_ref(), read_ahead).await?;
            Ok::<_, DeriveError>(file.map(|f| (path, f)))
        })
        .buffered(read_ahead)
        .try_collect()
        .await?;
    Ok(BlameNode {
        files: files.into_iter().flatten().collect(),
    })
}

#[async_trait]
impl Derivable for Blame {
    const NAME: &'static str = "blame";
    const VERSION: u32 = 2;
    // A node holds only the files its changeset changed, attributed along
    // each file's own version chain: the parents' nodes are not what it is
    // built from.
    const READS_PARENTS: bool = false;

    async fn derive(ctx: &Deriver<'_>, cs: &Changeset, _parents: &[Self]) -> Result<Self> {
        Self::put(ctx, node(ctx, cs, READ_AHEAD).await?).await
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
        let Some(last_changed) = Self::last_changed(ctx, cs, path).await? else {
            return Ok(None);
        };
        let blame = ctx.derive::<Blame>(last_changed).await?;
        blame.file(ctx, path).await.map(Some)
    }

    /// The attribution of `path` in the stored node of `last_changed`, the
    /// changeset that last changed it, or `None` if that node is not stored.
    pub async fn stored_file(
        ctx: &Deriver<'_>,
        last_changed: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<FileBlame>> {
        match ctx.fetch::<Blame>(last_changed).await? {
            Some(blame) => blame.file(ctx, path).await.map(Some),
            None => Ok(None),
        }
    }

    /// The attribution of `path` as `last_changed`, the changeset that last
    /// changed it, left it: what its node holds for `path`, attributed along
    /// the file's own version chain without deriving or storing the node,
    /// so without waiting on the other files that changeset changed.
    pub async fn file_alone(
        ctx: &Deriver<'_>,
        last_changed: ChangesetId,
        path: &RepoPath,
    ) -> Result<FileBlame> {
        let changeset = ctx
            .meta
            .get_changeset(ctx.repo, last_changed)
            .await?
            .ok_or(DeriveError::MissingChangeset(last_changed))?;
        let corrupt = DeriveError::CorruptPointer {
            ty: PathHistory::NAME,
            cs: last_changed,
        };
        let Some(FileChange::Tracked {
            content,
            kind,
            size,
            copy_from,
        }) = changeset.file_changes.get(path)
        else {
            return Err(corrupt);
        };
        let entry = TreeEntry::Blob {
            id: *content,
            kind: *kind,
            size: *size,
        };
        file_blame(ctx, &changeset, path, entry, copy_from.as_ref(), READ_AHEAD)
            .await?
            .ok_or(corrupt)
    }

    /// Derive and store the node of `cs` unless it is stored, as
    /// [`Deriver::derive`] does, but one file and one version at a time: for
    /// a node nothing waits on, so that deriving it keeps to about one core
    /// beside the requests being served.
    pub async fn store_gently(ctx: &Deriver<'_>, cs: ChangesetId) -> Result<Self> {
        if let Some(blame) = ctx.fetch::<Blame>(cs).await? {
            return Ok(blame);
        }
        let changeset = ctx
            .meta
            .get_changeset(ctx.repo, cs)
            .await?
            .ok_or(DeriveError::MissingChangeset(cs))?;
        let blame = Self::put(ctx, node(ctx, &changeset, 1).await?).await?;
        ctx.meta
            .put_derived(ctx.repo, Self::NAME, Self::VERSION, cs, blame.to_pointer())
            .await?;
        Ok(blame)
    }

    /// Store `node` in the object store.
    async fn put(ctx: &Deriver<'_>, node: BlameNode) -> Result<Self> {
        Ok(Self(
            ctx.objects.put_blob(Bytes::from(node.encode())).await?,
        ))
    }

    /// The changeset that last changed the file `path` at `cs`, if there is
    /// one.
    pub async fn last_changed(
        ctx: &Deriver<'_>,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<ChangesetId>> {
        let history = ctx.derive::<PathHistory>(cs).await?;
        Ok(match history.lookup(ctx, path).await? {
            Some(HistoryEntry::File(Change { last_changed, .. })) => Some(last_changed),
            _ => None,
        })
    }

    /// The attribution of `path` in this node.
    async fn file(self, ctx: &Deriver<'_>, path: &RepoPath) -> Result<FileBlame> {
        let node = BlameNode::load(ctx.objects, self.0).await?;
        node.files
            .get(path)
            .cloned()
            .ok_or(DeriveError::CorruptObject(self.0))
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
    fn line_count_matches_the_diff() {
        for (bytes, lines) in [
            (&b""[..], 0),
            (b"a", 1),
            (b"a\n", 1),
            (b"a\nb", 2),
            (b"a\nb\n", 2),
            (b"\n\n", 2),
        ] {
            assert_eq!(line_count(bytes), lines);
            assert_eq!(u64::from(script(bytes, b"").lines), lines);
        }
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
        let store = MemoryStore::with_repo(REPO);
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
        let store = MemoryStore::with_repo(REPO);
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
        let store = MemoryStore::with_repo(REPO);
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
        let meta = CountingMeta::new(&store);
        let d = Deriver::new(REPO, &meta, &counting);
        let Blame(node) = d.derive::<Blame>(c1).await.unwrap();
        let node = BlameNode::load(&store, node).await.unwrap();
        assert_eq!(node.files.len(), 1);
        // The previous version is read through c0's data; the only write is
        // c1's blame node.
        assert_eq!(counting.blob_puts(), 1);
        assert_eq!(meta.tree_puts(), 0);
        assert_eq!(meta.page_puts(), 0);
    }

    #[tokio::test]
    async fn cold_blame_reads_the_file_versions_not_the_history() {
        const N: usize = 900;
        const EVERY: usize = 3;
        let store = MemoryStore::with_repo(REPO);
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

    /// Phase 10.7: a file's attribution alone reads that file's versions,
    /// not those of the other files its changeset changed, and stores
    /// nothing; the node, stored gently afterwards, holds the same
    /// attribution.
    #[tokio::test]
    async fn file_alone_reads_only_its_own_versions() {
        const N: usize = 400;
        let store = MemoryStore::with_repo(REPO);
        let (hot, busy) = (
            RepoPath::new("hot.rs").unwrap(),
            RepoPath::new("busy.rs").unwrap(),
        );
        let mut ids: Vec<ChangesetId> = Vec::new();
        let mut versions = Vec::new();
        let (mut hot_text, mut busy_text) = (String::new(), String::new());
        for i in 0..N {
            // `busy.rs` changes every time, `hot.rs` every 50th time; the
            // last changeset changes both.
            let mut ch = BTreeMap::new();
            busy_text += &(i.to_string() + "\n");
            ch.insert(
                busy.clone(),
                tracked_bytes(&store, busy_text.as_bytes(), FileType::Regular, None).await,
            );
            if i % 50 == 0 || i == N - 1 {
                hot_text += &(i.to_string() + "\n");
                ch.insert(
                    hot.clone(),
                    tracked_bytes(&store, hot_text.as_bytes(), FileType::Regular, None).await,
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
            if i % 50 == 0 || i == N - 1 {
                versions.push(id);
            }
            ids.push(id);
        }
        let head = ids[N - 1];
        Deriver::new(REPO, &store, &store)
            .derive::<PathHistory>(head)
            .await
            .unwrap();

        let meta = CountingMeta::new(&store);
        let d = Deriver::new(REPO, &meta, &store);
        let alone = Blame::file_alone(&d, head, &hot).await.unwrap();
        assert_eq!(alone.lines().collect::<Vec<_>>(), versions);
        let k = versions.len();
        assert!(
            meta.changeset_reads() <= k + 1,
            "changesets read: {} for {k} versions",
            meta.changeset_reads()
        );
        assert_eq!(d.fetch::<Blame>(head).await.unwrap(), None);
        assert_eq!(Blame::stored_file(&d, head, &hot).await.unwrap(), None);

        let node = Blame::store_gently(&d, head).await.unwrap();
        assert!(meta.changeset_reads() >= N, "the node walks busy.rs too");
        assert_eq!(d.fetch::<Blame>(head).await.unwrap(), Some(node));
        // Gently or not, the node is the same.
        let cs = store.get_changeset(REPO, head).await.unwrap().unwrap();
        let full = Blame::put(&d, super::node(&d, &cs, READ_AHEAD).await.unwrap());
        assert_eq!(full.await.unwrap(), node);
        assert_eq!(
            Blame::stored_file(&d, head, &hot).await.unwrap(),
            Some(alone)
        );
    }

    // ---- property test: diff attribution ≡ tagged-line model ------------

    #[derive(Debug, Clone)]
    struct Step {
        deletes: Vec<prop::sample::Index>,
        inserts: Vec<prop::sample::Index>,
        trailing_newline: bool,
        rename: bool,
        check: bool,
    }

    fn arb_step() -> impl Strategy<Value = Step> {
        (
            prop::collection::vec(any::<prop::sample::Index>(), 0..3),
            prop::collection::vec(any::<prop::sample::Index>(), 0..4),
            any::<bool>(),
            prop::bool::weighted(0.05),
            prop::bool::weighted(0.1),
        )
            .prop_map(|(deletes, inserts, trailing_newline, rename, check)| Step {
                deletes,
                inserts,
                trailing_newline,
                rename,
                check,
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn attribution_equals_model(steps in prop::collection::vec(arb_step(), 1..48)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let store = MemoryStore::with_repo(REPO);
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

                    // Blame is derived only now and then, so a chain often
                    // spans several anchors and read batches.
                    if !step.check && i + 1 != steps.len() {
                        continue;
                    }
                    let last = Blame::last_changed(&d, id, &path).await.unwrap().expect("file exists");
                    let alone = Blame::file_alone(&d, last, &path).await.unwrap();
                    prop_assert_eq!(alone.lines().collect::<Vec<_>>(), model.tags(), "file alone at step {}", i);
                    let got = Blame::for_path(&d, id, &path).await.unwrap().expect("file exists");
                    prop_assert_eq!(got.line_count(), line_count(&content), "line count at step {}", i);
                    prop_assert_eq!(got.lines().collect::<Vec<_>>(), model.tags(), "attribution at step {}", i);
                }
                Ok(())
            })?;
        }
    }
}
