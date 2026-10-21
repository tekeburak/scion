//! File-level three-way merge, and the two operations built on it.
//!
//! [`three_way`] combines two sets of changes made from a common base tree
//! and returns the result as `file_changes` **relative to `ours`**, which is
//! exactly the change list of the changeset a server would create:
//!
//! * [`merge`]: `ours` and `theirs` are two heads, the base is their
//!   `SkipList::merge_base`; the result is the merge changeset's
//!   `file_changes` (first parent `ours`).
//! * [`rebase`]: `theirs` is one changeset, the base is its first parent,
//!   `ours` is the new parent; the result is the rebased changeset's
//!   `file_changes`. The changeset's own `file_changes` (with `copy_from`)
//!   are used as its side of the merge, so renames survive a rebase.
//!
//! Rules per path (`b` base, `o` ours, `t` theirs):
//!
//! | ours      | theirs    | result                                              |
//! |-----------|-----------|-----------------------------------------------------|
//! | unchanged | changed   | theirs' change                                      |
//! | changed   | unchanged | nothing (already in ours)                           |
//! | deleted   | deleted   | nothing                                             |
//! | set X     | set X     | nothing                                             |
//! | set X     | set Y     | line merge of the contents; kind from whichever side changed it |
//! | deleted   | set       | conflict (delete/modify), either way round          |
//!
//! A change that makes a path a file while the other side put files beneath
//! it (or vice versa) is a shape conflict. Conflicts never produce changes;
//! the caller decides what to do with them (Phase 5 writes the conflicted
//! content into the working copy).

use std::collections::BTreeMap;

use bytes::Bytes;
use scion_derived::{Deriver, RootTree, SkipList};
use scion_store::ObjectStore;
use scion_types::{BlobId, ChangesetId, FileChange, FileType, RepoPath, TreeEntry, TreeId};

use crate::{DiffEntry, Error, FileEntry, Result, tree_diff};

/// Which side of a merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

/// A path the merge could not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// Both sides changed the content differently; `content` has markers.
    Content {
        path: RepoPath,
        content: Vec<u8>,
        conflicts: usize,
    },
    /// One side deleted the file, the other changed it.
    DeleteModify { path: RepoPath, deleted_by: Side },
    /// Both sides changed the file kind (regular/executable/symlink) differently.
    Kind { path: RepoPath },
    /// One side made `path` a file while the other put files beneath it
    /// (`other` is one such path), or the other way round.
    Shape { path: RepoPath, other: RepoPath },
}

impl Conflict {
    /// The path this conflict is about.
    #[must_use]
    pub fn path(&self) -> &RepoPath {
        match self {
            Self::Content { path, .. }
            | Self::DeleteModify { path, .. }
            | Self::Kind { path }
            | Self::Shape { path, .. } => path,
        }
    }
}

/// Outcome of a three-way merge: changes relative to `ours`, plus conflicts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreeWay {
    pub changes: BTreeMap<RepoPath, FileChange>,
    pub conflicts: Vec<Conflict>,
}

impl ThreeWay {
    /// Whether the merge resolved every path.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// One side's change to a path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    Deleted,
    Set(FileEntry, Option<(RepoPath, ChangesetId)>),
}

fn changes_from_diff(entries: Vec<DiffEntry>) -> BTreeMap<RepoPath, Change> {
    entries
        .into_iter()
        .map(|e| match e {
            DiffEntry::Added { path, new } | DiffEntry::Modified { path, new, .. } => {
                (path, Change::Set(new, None))
            }
            DiffEntry::Deleted { path, .. } => (path, Change::Deleted),
        })
        .collect()
}

fn changes_from_changeset(changes: &BTreeMap<RepoPath, FileChange>) -> BTreeMap<RepoPath, Change> {
    changes
        .iter()
        .map(|(path, change)| {
            let c = match change {
                FileChange::Deleted => Change::Deleted,
                FileChange::Tracked {
                    content,
                    kind,
                    size,
                    copy_from,
                } => Change::Set(
                    FileEntry {
                        id: *content,
                        kind: *kind,
                        size: *size,
                    },
                    copy_from.clone(),
                ),
            };
            (path.clone(), c)
        })
        .collect()
}

fn tracked(entry: FileEntry, copy_from: Option<(RepoPath, ChangesetId)>) -> FileChange {
    FileChange::Tracked {
        content: entry.id,
        kind: entry.kind,
        size: entry.size,
        copy_from,
    }
}

async fn blob(objects: &dyn ObjectStore, id: BlobId) -> Result<Bytes> {
    objects.get_blob(id).await?.ok_or(Error::MissingObject(id))
}

/// Whether `a` is a proper ancestor directory of `b`.
fn is_ancestor_path(a: &RepoPath, b: &RepoPath) -> bool {
    b.as_str().len() > a.as_str().len()
        && b.as_str().starts_with(a.as_str())
        && b.as_str().as_bytes()[a.as_str().len()] == b'/'
}

fn kind_of(entry: Option<&TreeEntry>) -> Option<FileType> {
    match entry {
        Some(TreeEntry::Blob { kind, .. }) => Some(*kind),
        _ => None,
    }
}

/// Merge `theirs` (changes from `base`) into `ours` (a tree that also
/// descends from `base`). Returns changes relative to `ours`.
async fn three_way(
    ctx: &Deriver<'_>,
    base: Option<TreeId>,
    ours: TreeId,
    theirs: &BTreeMap<RepoPath, Change>,
) -> Result<ThreeWay> {
    let objects = ctx.objects;
    let ours_changes = changes_from_diff(tree_diff(ctx, base, Some(ours)).await?);
    let base_root = base.map(RootTree);
    let mut out = ThreeWay::default();

    for (path, their) in theirs {
        // Shape conflicts: a file on one side where the other side has files beneath.
        let shape = match their {
            Change::Set(..) => ours_changes.iter().find(|(p, c)| {
                matches!(c, Change::Set(..))
                    && (is_ancestor_path(p, path) || is_ancestor_path(path, p))
            }),
            Change::Deleted => None,
        };
        if let Some((other, _)) = shape {
            out.conflicts.push(Conflict::Shape {
                path: path.clone(),
                other: other.clone(),
            });
            continue;
        }

        match (ours_changes.get(path), their) {
            // Only theirs touched it.
            (None, Change::Deleted) => {
                out.changes.insert(path.clone(), FileChange::Deleted);
            }
            (None, Change::Set(entry, copy_from)) => {
                out.changes
                    .insert(path.clone(), tracked(*entry, copy_from.clone()));
            }
            // Both deleted, or both set the same thing: ours already has it.
            (Some(Change::Deleted), Change::Deleted) => {}
            (Some(Change::Set(o, _)), Change::Set(t, _)) if o == t => {}
            (Some(Change::Deleted), Change::Set(..)) => {
                out.conflicts.push(Conflict::DeleteModify {
                    path: path.clone(),
                    deleted_by: Side::Ours,
                });
            }
            (Some(Change::Set(..)), Change::Deleted) => {
                out.conflicts.push(Conflict::DeleteModify {
                    path: path.clone(),
                    deleted_by: Side::Theirs,
                });
            }
            (Some(Change::Set(o, _)), Change::Set(t, _)) => {
                let base_entry = match &base_root {
                    Some(root) => root.lookup(ctx, path).await?,
                    None => None,
                };
                let base_kind = kind_of(base_entry.as_ref());
                let kind = if o.kind == t.kind {
                    o.kind
                } else if Some(o.kind) == base_kind {
                    t.kind
                } else if Some(t.kind) == base_kind {
                    o.kind
                } else {
                    out.conflicts.push(Conflict::Kind { path: path.clone() });
                    continue;
                };
                let base_bytes = match base_entry {
                    Some(TreeEntry::Blob { id, .. }) => blob(objects, id).await?,
                    _ => Bytes::new(),
                };
                let ours_bytes = blob(objects, o.id).await?;
                let theirs_bytes = blob(objects, t.id).await?;
                match scion_diff3::merge(&base_bytes, &ours_bytes, &theirs_bytes) {
                    scion_diff3::Merged::Clean(content) => {
                        let size = u64::try_from(content.len()).expect("fits");
                        let id = objects.put_blob(Bytes::from(content)).await?;
                        let merged = FileEntry { id, kind, size };
                        if merged != *o {
                            out.changes.insert(path.clone(), tracked(merged, None));
                        }
                    }
                    scion_diff3::Merged::Conflicted { content, conflicts } => {
                        out.conflicts.push(Conflict::Content {
                            path: path.clone(),
                            content,
                            conflicts,
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Merge head `theirs` into head `ours`; the result is the `file_changes` of
/// a merge changeset with parents `[ours, theirs]`. Unrelated heads merge
/// against the empty tree. A file taken from `theirs` as it is there names
/// itself in `theirs` as `copy_from`, so blame follows `theirs`' history
/// for it while history stays on the first-parent line.
pub async fn merge(ctx: &Deriver<'_>, ours: ChangesetId, theirs: ChangesetId) -> Result<ThreeWay> {
    let base = match SkipList::merge_base(ctx, ours, theirs).await? {
        Some(b) => Some(ctx.derive::<RootTree>(b).await?.0),
        None => None,
    };
    let ours_tree = ctx.derive::<RootTree>(ours).await?.0;
    let theirs_tree = ctx.derive::<RootTree>(theirs).await?.0;
    let theirs_changes = changes_from_diff(tree_diff(ctx, base, Some(theirs_tree)).await?)
        .into_iter()
        .map(|(path, change)| match change {
            Change::Set(entry, _) => {
                let from = Some((path.clone(), theirs));
                (path, Change::Set(entry, from))
            }
            Change::Deleted => (path, Change::Deleted),
        })
        .collect();
    three_way(ctx, base, ours_tree, &theirs_changes).await
}

/// Replay changeset `cs` on top of `onto`; the result is the `file_changes`
/// of the rebased changeset with parent `[onto]`. `cs`'s own change list is
/// its side of the merge, so `copy_from` provenance is kept.
pub async fn rebase(ctx: &Deriver<'_>, cs: ChangesetId, onto: ChangesetId) -> Result<ThreeWay> {
    let changeset = ctx
        .meta
        .get_changeset(ctx.repo, cs)
        .await?
        .ok_or(Error::MissingChangeset(cs))?;
    let base = match changeset.first_parent() {
        Some(p) => Some(ctx.derive::<RootTree>(p).await?.0),
        None => None,
    };
    let onto_tree = ctx.derive::<RootTree>(onto).await?.0;
    let theirs = changes_from_changeset(&changeset.file_changes);
    three_way(ctx, base, onto_tree, &theirs).await
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use scion_derived::test_util::{
        Flat, REPO, arb_changes, changeset, materialize, tracked as tracked_change,
    };
    use scion_store::{MemoryStore, MetadataStore};

    use super::*;

    // ---- naive reference -----------------------------------------------

    type File = (BlobId, FileType, u64);

    enum NaiveOutcome {
        Clean(Flat),
        /// Paths that conflict (content, delete/modify, kind, or shape).
        Conflicts(Vec<RepoPath>),
    }

    /// Per-path three-way rule on flat maps, then a shape check.
    async fn naive_merge(
        objects: &dyn ObjectStore,
        base: &Flat,
        ours: &Flat,
        theirs: &Flat,
    ) -> NaiveOutcome {
        let mut paths: Vec<&RepoPath> = base
            .keys()
            .chain(ours.keys())
            .chain(theirs.keys())
            .collect();
        paths.sort();
        paths.dedup();
        let mut result = Flat::new();
        let mut conflicts = Vec::new();
        for path in paths {
            let (b, o, t) = (base.get(path), ours.get(path), theirs.get(path));
            let chosen: Option<File> = if o == t {
                o.copied()
            } else if o == b {
                t.copied()
            } else if t == b {
                o.copied()
            } else if let (Some(o), Some(t)) = (o, t) {
                let bk = b.map(|f| f.1);
                let kind = if o.1 == t.1 {
                    Some(o.1)
                } else if Some(o.1) == bk {
                    Some(t.1)
                } else if Some(t.1) == bk {
                    Some(o.1)
                } else {
                    None
                };
                let Some(kind) = kind else {
                    conflicts.push(path.clone());
                    continue;
                };
                let bb = match b {
                    Some(f) => objects.get_blob(f.0).await.unwrap().unwrap(),
                    None => Bytes::new(),
                };
                let ob = objects.get_blob(o.0).await.unwrap().unwrap();
                let tb = objects.get_blob(t.0).await.unwrap().unwrap();
                match scion_diff3::merge(&bb, &ob, &tb) {
                    scion_diff3::Merged::Clean(c) => {
                        let id = BlobId::hash(&c);
                        Some((id, kind, u64::try_from(c.len()).unwrap()))
                    }
                    scion_diff3::Merged::Conflicted { .. } => {
                        conflicts.push(path.clone());
                        continue;
                    }
                }
            } else {
                conflicts.push(path.clone());
                continue;
            };
            if let Some(f) = chosen {
                result.insert(path.clone(), f);
            }
        }
        // Shape: a file and files beneath it cannot coexist.
        let keys: Vec<&RepoPath> = result.keys().collect();
        for a in &keys {
            for b in &keys {
                if is_ancestor_path(a, b) {
                    conflicts.push((*a).clone());
                }
            }
        }
        if conflicts.is_empty() {
            NaiveOutcome::Clean(result)
        } else {
            conflicts.sort();
            conflicts.dedup();
            NaiveOutcome::Conflicts(conflicts)
        }
    }

    // ---- helpers ---------------------------------------------------------

    async fn put(
        store: &MemoryStore,
        parents: Vec<ChangesetId>,
        changes: BTreeMap<RepoPath, FileChange>,
        tag: &str,
    ) -> ChangesetId {
        store
            .put_changeset(&changeset(parents, changes, tag))
            .await
            .unwrap()
    }

    async fn content_change(store: &MemoryStore, bytes: &[u8]) -> FileChange {
        store.put_blob(Bytes::copy_from_slice(bytes)).await.unwrap();
        tracked_change(bytes)
    }

    async fn flat_of(d: &Deriver<'_>, store: &MemoryStore, cs: ChangesetId) -> Flat {
        let RootTree(t) = d.derive::<RootTree>(cs).await.unwrap();
        materialize(store, t).await
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    // ---- tests -----------------------------------------------------------

    #[tokio::test]
    async fn merge_combines_disjoint_edits_and_line_merges_shared_files() {
        let store = MemoryStore::with_repo(REPO);
        let d = Deriver::new(REPO, &store, &store);
        let mut c = BTreeMap::new();
        c.insert(
            p("shared"),
            content_change(&store, b"a\nb\nc\nd\ne\n").await,
        );
        c.insert(p("only-ours"), content_change(&store, b"o\n").await);
        c.insert(p("gone"), content_change(&store, b"g\n").await);
        let base = put(&store, vec![], c, "base").await;

        let mut c = BTreeMap::new();
        c.insert(
            p("shared"),
            content_change(&store, b"A\nb\nc\nd\ne\n").await,
        );
        c.insert(p("only-ours"), content_change(&store, b"O\n").await);
        let ours = put(&store, vec![base], c, "ours").await;

        let mut c = BTreeMap::new();
        c.insert(
            p("shared"),
            content_change(&store, b"a\nb\nc\nd\nE\n").await,
        );
        c.insert(p("new"), content_change(&store, b"n\n").await);
        c.insert(p("gone"), FileChange::Deleted);
        let theirs = put(&store, vec![base], c, "theirs").await;

        let m = merge(&d, ours, theirs).await.unwrap();
        assert!(m.is_clean(), "{:?}", m.conflicts);
        let merged = put(&store, vec![ours, theirs], m.changes.clone(), "merge").await;
        let flat = flat_of(&d, &store, merged).await;
        assert_eq!(
            flat.keys().map(RepoPath::as_str).collect::<Vec<_>>(),
            ["new", "only-ours", "shared"]
        );
        let shared = store.get_blob(flat[&p("shared")].0).await.unwrap().unwrap();
        assert_eq!(&shared[..], b"A\nb\nc\nd\nE\n");
        // only-ours was not in the change list: it is already in ours.
        assert!(!m.changes.contains_key(&p("only-ours")));
        assert_eq!(m.changes.get(&p("gone")), Some(&FileChange::Deleted));
    }

    #[tokio::test]
    async fn merge_reports_each_conflict_kind() {
        let store = MemoryStore::with_repo(REPO);
        let d = Deriver::new(REPO, &store, &store);
        let mut c = BTreeMap::new();
        c.insert(p("content"), content_change(&store, b"x\n").await);
        c.insert(p("delmod"), content_change(&store, b"x\n").await);
        c.insert(p("kind"), content_change(&store, b"x\n").await);
        let base = put(&store, vec![], c, "base").await;

        let mut c = BTreeMap::new();
        c.insert(p("content"), content_change(&store, b"ours\n").await);
        c.insert(p("delmod"), FileChange::Deleted);
        c.insert(
            p("kind"),
            FileChange::Tracked {
                content: BlobId::hash(b"x\n"),
                kind: FileType::Executable,
                size: 2,
                copy_from: None,
            },
        );
        c.insert(p("dir"), content_change(&store, b"file\n").await);
        let ours = put(&store, vec![base], c, "ours").await;

        let mut c = BTreeMap::new();
        c.insert(p("content"), content_change(&store, b"theirs\n").await);
        c.insert(p("delmod"), content_change(&store, b"changed\n").await);
        c.insert(
            p("kind"),
            FileChange::Tracked {
                content: BlobId::hash(b"x\n"),
                kind: FileType::Symlink,
                size: 2,
                copy_from: None,
            },
        );
        c.insert(p("dir/inner"), content_change(&store, b"i\n").await);
        let theirs = put(&store, vec![base], c, "theirs").await;

        let m = merge(&d, ours, theirs).await.unwrap();
        assert!(m.changes.is_empty());
        let mut kinds: Vec<String> = m
            .conflicts
            .iter()
            .map(|c| match c {
                Conflict::Content {
                    path,
                    conflicts,
                    content,
                } => {
                    assert_eq!(*conflicts, 1);
                    assert!(content.starts_with(b"<<<<<<< ours\nours\n"));
                    format!("content:{path}")
                }
                Conflict::DeleteModify { path, deleted_by } => {
                    format!("delmod:{path}:{deleted_by:?}")
                }
                Conflict::Kind { path } => format!("kind:{path}"),
                Conflict::Shape { path, other } => format!("shape:{path}:{other}"),
            })
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            [
                "content:content",
                "delmod:delmod:Ours",
                "kind:kind",
                "shape:dir/inner:dir"
            ]
        );
    }

    #[tokio::test]
    async fn rebase_replays_changes_and_keeps_copy_from() {
        let store = MemoryStore::with_repo(REPO);
        let d = Deriver::new(REPO, &store, &store);
        let mut c = BTreeMap::new();
        c.insert(p("a"), content_change(&store, b"1\n2\n3\n").await);
        c.insert(p("b"), content_change(&store, b"b\n").await);
        let base = put(&store, vec![], c, "base").await;

        // main moves on: edits a's first line.
        let mut c = BTreeMap::new();
        c.insert(p("a"), content_change(&store, b"ONE\n2\n3\n").await);
        let main = put(&store, vec![base], c, "main").await;

        // feature: edits a's last line and renames b -> c.
        let mut c = BTreeMap::new();
        c.insert(p("a"), content_change(&store, b"1\n2\nTHREE\n").await);
        c.insert(p("b"), FileChange::Deleted);
        c.insert(
            p("c"),
            FileChange::Tracked {
                content: BlobId::hash(b"b\n"),
                kind: FileType::Regular,
                size: 2,
                copy_from: Some((p("b"), base)),
            },
        );
        let feature = put(&store, vec![base], c, "feature").await;

        let r = rebase(&d, feature, main).await.unwrap();
        assert!(r.is_clean(), "{:?}", r.conflicts);
        assert!(matches!(
            r.changes.get(&p("c")),
            Some(FileChange::Tracked { copy_from: Some((from, at)), .. }) if *from == p("b") && *at == base
        ));
        assert_eq!(r.changes.get(&p("b")), Some(&FileChange::Deleted));

        let rebased = put(&store, vec![main], r.changes, "feature'").await;
        let flat = flat_of(&d, &store, rebased).await;
        let a = store.get_blob(flat[&p("a")].0).await.unwrap().unwrap();
        assert_eq!(&a[..], b"ONE\n2\nTHREE\n");
        assert!(flat.contains_key(&p("c")) && !flat.contains_key(&p("b")));

        // Rebasing onto its own parent reproduces the changeset exactly.
        let same = rebase(&d, feature, base).await.unwrap();
        let original = store.get_changeset(REPO, feature).await.unwrap().unwrap();
        assert_eq!(same.changes, original.file_changes);
    }

    // ---- property test: merge ≡ naive flat-map three-way -----------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn merge_equals_naive(
            base_steps in prop::collection::vec(arb_changes(), 1..4),
            ours_steps in prop::collection::vec(arb_changes(), 1..3),
            theirs_steps in prop::collection::vec(arb_changes(), 1..3),
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let store = MemoryStore::with_repo(REPO);
                let d = Deriver::new(REPO, &store, &store);
                // Blobs must exist for line merges; tracked() only hashes.
                let materialise_blobs = |changes: &BTreeMap<RepoPath, FileChange>| {
                    let store = &store;
                    let changes = changes.clone();
                    async move {
                        for change in changes.values() {
                            if let FileChange::Tracked { content, .. } = change {
                                // tracked(&[b]) hashes a one-byte payload; recover it.
                                for b in 0..=u8::MAX {
                                    if BlobId::hash(&[b]) == *content {
                                        store.put_blob(Bytes::copy_from_slice(&[b])).await.unwrap();
                                        break;
                                    }
                                }
                            }
                        }
                    }
                };
                let mut head: Option<ChangesetId> = None;
                for (i, c) in base_steps.iter().enumerate() {
                    materialise_blobs(c).await;
                    head = Some(put(&store, head.into_iter().collect(), c.clone(), &format!("b{i}")).await);
                }
                let base = head.unwrap();
                let mut ours = base;
                for (i, c) in ours_steps.iter().enumerate() {
                    materialise_blobs(c).await;
                    ours = put(&store, vec![ours], c.clone(), &format!("o{i}")).await;
                }
                let mut theirs = base;
                for (i, c) in theirs_steps.iter().enumerate() {
                    materialise_blobs(c).await;
                    theirs = put(&store, vec![theirs], c.clone(), &format!("t{i}")).await;
                }

                let (fb, fo, ft) = (flat_of(&d, &store, base).await, flat_of(&d, &store, ours).await, flat_of(&d, &store, theirs).await);
                let expected = naive_merge(&store, &fb, &fo, &ft).await;
                let got = merge(&d, ours, theirs).await.unwrap();
                match expected {
                    NaiveOutcome::Clean(flat) => {
                        prop_assert!(got.is_clean(), "unexpected conflicts {:?}", got.conflicts);
                        let merged = put(&store, vec![ours, theirs], got.changes.clone(), "m").await;
                        prop_assert_eq!(flat_of(&d, &store, merged).await, flat);
                    }
                    NaiveOutcome::Conflicts(paths) => {
                        let mut got_paths: Vec<RepoPath> = got.conflicts.iter().map(|c| c.path().clone()).collect();
                        // A shape conflict names theirs' path; the naive check names the ancestor file.
                        for c in &got.conflicts {
                            if let Conflict::Shape { other, .. } = c {
                                got_paths.push(other.clone());
                            }
                        }
                        got_paths.sort();
                        got_paths.dedup();
                        prop_assert!(!got.is_clean(), "expected conflicts at {:?}", paths);
                        // Every naive conflict is reported (possibly under a related path).
                        for path in &paths {
                            prop_assert!(
                                got_paths.iter().any(|g| g == path || is_ancestor_path(g, path) || is_ancestor_path(path, g)),
                                "naive conflict {} not reported in {:?}", path, got_paths
                            );
                        }
                    }
                }
                Ok(())
            })?;
        }
    }
}
