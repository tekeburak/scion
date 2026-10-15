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
use scion_store::ObjectStore;
use scion_types::canonical::{Reader, Writer};
use scion_types::{BlobId, Changeset, ChangesetId, FileChange, RepoPath, TreeEntry};

use crate::{Derivable, DeriveError, Deriver, HistoryEntry, PathHistory, Result, RootTree};

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

    async fn store(&self, objects: &dyn ObjectStore) -> Result<BlobId> {
        Ok(objects.put_blob(Bytes::from(self.encode())).await?)
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

#[async_trait]
impl Derivable for Blame {
    const NAME: &'static str = "blame";
    const VERSION: u32 = 1;

    async fn derive(ctx: &Deriver<'_>, cs: &Changeset, _parents: &[Self]) -> Result<Self> {
        let cs_id = cs.id();
        let mut node = BlameNode::default();
        for (path, change) in &cs.file_changes {
            let FileChange::Tracked {
                content,
                kind,
                size,
                copy_from,
            } = change
            else {
                continue;
            };
            let new_entry = TreeEntry::Blob {
                id: *content,
                kind: *kind,
                size: *size,
            };

            let source = match copy_from {
                Some((from, at)) => Some((*at, from.clone())),
                None => cs.first_parent().map(|p| (p, path.clone())),
            };
            let previous = match &source {
                Some((at, from)) => file_at(ctx, *at, from).await?,
                None => None,
            };
            // Same entry at the same path: the tree did not change here, so
            // PathHistory keeps pointing at the older changeset.
            if copy_from.is_none() && previous.as_ref().is_some_and(|(e, _, _)| *e == new_entry) {
                continue;
            }

            let new_bytes = ctx
                .objects
                .get_blob(*content)
                .await?
                .ok_or(DeriveError::MissingObject(*content))?;
            let file = match previous {
                None => FileBlame::uniform(cs_id, line_count(&new_bytes)),
                Some((TreeEntry::Blob { id, .. }, old_blame, node_id)) => {
                    let old_bytes = ctx
                        .objects
                        .get_blob(id)
                        .await?
                        .ok_or(DeriveError::MissingObject(id))?;
                    attribute(&old_bytes, &old_blame, &new_bytes, cs_id)
                        .ok_or(DeriveError::CorruptObject(node_id))?
                }
                Some((TreeEntry::Tree { .. }, ..)) => unreachable!("file_at returns files only"),
            };
            node.files.insert(path.clone(), file);
        }
        Ok(Self(node.store(ctx.objects).await?))
    }

    fn to_pointer(&self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }

    fn from_pointer(bytes: &[u8]) -> Option<Self> {
        BlobId::try_from_slice(bytes).ok().map(Self)
    }
}

/// The file entry, its attribution and the blame node it came from, for
/// `path` at changeset `at`; `None` if there is no file there.
async fn file_at(
    ctx: &Deriver<'_>,
    at: ChangesetId,
    path: &RepoPath,
) -> Result<Option<(TreeEntry, FileBlame, BlobId)>> {
    let root = ctx.derive::<RootTree>(at).await?;
    let Some(entry @ TreeEntry::Blob { .. }) = root.lookup(ctx.objects, path).await? else {
        return Ok(None);
    };
    let history = ctx.derive::<PathHistory>(at).await?;
    let Some(HistoryEntry::File { last_changed }) = history.lookup(ctx.objects, path).await? else {
        return Err(DeriveError::CorruptObject(history.0.page));
    };
    let blame = ctx.derive::<Blame>(last_changed).await?;
    let node = BlameNode::load(ctx.objects, blame.0).await?;
    let file = node
        .files
        .get(path)
        .cloned()
        .ok_or(DeriveError::CorruptObject(blame.0))?;
    Ok(Some((entry, file, blame.0)))
}

impl Blame {
    /// The attribution of `path` at `cs`, or `None` if there is no file there.
    pub async fn for_path(
        ctx: &Deriver<'_>,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<FileBlame>> {
        let history = ctx.derive::<PathHistory>(cs).await?;
        let Some(HistoryEntry::File { last_changed }) = history.lookup(ctx.objects, path).await?
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
    use crate::test_util::{Counting, REPO, changeset};

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
