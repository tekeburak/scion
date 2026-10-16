//! Source-control algorithms over the stores and the derived data.
//!
//! Nothing here performs I/O of its own: every function takes a
//! [`Deriver`] (repository + stores) and works through the derived types in
//! `scion-derived`. Read-only operations live in this crate's root; merge and
//! rebase arrive in later phases.
//!
//! * [`tree_diff`] / [`diff`]: file-level differences between two trees or
//!   changesets, visiting only subtrees whose ids differ.
//! * [`log`]: the first-parent chain from a head, read from the `SkipList`
//!   index (no changeset rows are loaded).
//! * [`path_log`]: the changesets that changed a path, found by hopping
//!   through `PathHistory` (cost proportional to the number of changes to
//!   that path, never to the length of history).
//!
//! Merge-base and ancestry are `SkipList::merge_base` / `SkipList::is_ancestor`.

#![forbid(unsafe_code)]

mod merge;

pub use merge::{Conflict, Side, ThreeWay, merge, rebase};

use scion_derived::{Deriver, PathHistory, RootTree, SkipList};
use scion_store::ObjectStore;
use scion_types::{
    BlobId, ChangesetId, FileType, PathComponent, RepoPath, Tree, TreeEntry, TreeId,
};

pub use scion_derived::{DeriveError as Error, Result};

/// A file as it appears in a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileEntry {
    pub id: BlobId,
    pub kind: FileType,
    pub size: u64,
}

/// One file-level difference. Directories never appear: a directory that is
/// added or removed shows up as its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffEntry {
    Added {
        path: RepoPath,
        new: FileEntry,
    },
    Deleted {
        path: RepoPath,
        old: FileEntry,
    },
    Modified {
        path: RepoPath,
        old: FileEntry,
        new: FileEntry,
    },
}

impl DiffEntry {
    /// The path this entry is about.
    #[must_use]
    pub fn path(&self) -> &RepoPath {
        match self {
            Self::Added { path, .. } | Self::Deleted { path, .. } | Self::Modified { path, .. } => {
                path
            }
        }
    }
}

fn file_entry(entry: &TreeEntry) -> Option<FileEntry> {
    match entry {
        TreeEntry::Blob { id, kind, size } => Some(FileEntry {
            id: *id,
            kind: *kind,
            size: *size,
        }),
        TreeEntry::Tree { .. } => None,
    }
}

async fn load_tree(objects: &dyn ObjectStore, id: TreeId) -> Result<Tree> {
    objects.get_tree(id).await?.ok_or(Error::MissingTree(id))
}

fn join(prefix: Option<&RepoPath>, name: &PathComponent) -> RepoPath {
    match prefix {
        Some(p) => p.join(name),
        None => RepoPath::new(name.as_str()).expect("component is a valid path"),
    }
}

/// Every file under `tree`, as `Added` (`added == true`) or `Deleted` entries.
async fn all_files(
    objects: &dyn ObjectStore,
    prefix: Option<&RepoPath>,
    tree: TreeId,
    added: bool,
    out: &mut Vec<DiffEntry>,
) -> Result<()> {
    let mut stack = vec![(prefix.cloned(), tree)];
    while let Some((prefix, id)) = stack.pop() {
        for (name, entry) in load_tree(objects, id).await?.entries {
            let path = join(prefix.as_ref(), &name);
            match entry {
                TreeEntry::Tree { id } => stack.push((Some(path), id)),
                TreeEntry::Blob { .. } => {
                    let file = file_entry(&entry).expect("blob");
                    out.push(if added {
                        DiffEntry::Added { path, new: file }
                    } else {
                        DiffEntry::Deleted { path, old: file }
                    });
                }
            }
        }
    }
    Ok(())
}

async fn diff_trees(
    objects: &dyn ObjectStore,
    prefix: Option<&RepoPath>,
    old: TreeId,
    new: TreeId,
    out: &mut Vec<DiffEntry>,
) -> Result<()> {
    if old == new {
        return Ok(());
    }
    let old_tree = load_tree(objects, old).await?;
    let new_tree = load_tree(objects, new).await?;
    let names: std::collections::BTreeSet<&PathComponent> = old_tree
        .entries
        .keys()
        .chain(new_tree.entries.keys())
        .collect();
    for name in names {
        let path = join(prefix, name);
        match (old_tree.entries.get(name), new_tree.entries.get(name)) {
            (Some(TreeEntry::Tree { id: o }), Some(TreeEntry::Tree { id: n })) => {
                Box::pin(diff_trees(objects, Some(&path), *o, *n, out)).await?;
            }
            (Some(o @ TreeEntry::Blob { .. }), Some(n @ TreeEntry::Blob { .. })) => {
                if o != n {
                    out.push(DiffEntry::Modified {
                        path,
                        old: file_entry(o).expect("blob"),
                        new: file_entry(n).expect("blob"),
                    });
                }
            }
            (o, n) => {
                // Absent, or a file on one side and a directory on the other.
                match o {
                    Some(TreeEntry::Tree { id }) => {
                        all_files(objects, Some(&path), *id, false, out).await?;
                    }
                    Some(blob @ TreeEntry::Blob { .. }) => out.push(DiffEntry::Deleted {
                        path: path.clone(),
                        old: file_entry(blob).expect("blob"),
                    }),
                    None => {}
                }
                match n {
                    Some(TreeEntry::Tree { id }) => {
                        all_files(objects, Some(&path), *id, true, out).await?;
                    }
                    Some(blob @ TreeEntry::Blob { .. }) => out.push(DiffEntry::Added {
                        path,
                        new: file_entry(blob).expect("blob"),
                    }),
                    None => {}
                }
            }
        }
    }
    Ok(())
}

/// File-level differences between two trees (`None` = empty tree), sorted by
/// path. Subtrees with equal ids are skipped, so the cost is proportional to
/// the differing part of the trees.
pub async fn tree_diff(
    objects: &dyn ObjectStore,
    old: Option<TreeId>,
    new: Option<TreeId>,
) -> Result<Vec<DiffEntry>> {
    let mut out = Vec::new();
    match (old, new) {
        (None, None) => {}
        (Some(o), None) => all_files(objects, None, o, false, &mut out).await?,
        (None, Some(n)) => all_files(objects, None, n, true, &mut out).await?,
        (Some(o), Some(n)) => diff_trees(objects, None, o, n, &mut out).await?,
    }
    // Tree order is per directory; callers expect byte order of full paths.
    out.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(out)
}

/// File-level differences between two changesets (`old = None` diffs against
/// the empty tree).
pub async fn diff(
    ctx: &Deriver<'_>,
    old: Option<ChangesetId>,
    new: ChangesetId,
) -> Result<Vec<DiffEntry>> {
    let old_tree = match old {
        Some(cs) => Some(ctx.derive::<RootTree>(cs).await?.0),
        None => None,
    };
    let new_tree = ctx.derive::<RootTree>(new).await?.0;
    tree_diff(ctx.objects, old_tree, Some(new_tree)).await
}

/// Up to `limit` changesets along the first-parent chain, `head` first.
pub async fn log(ctx: &Deriver<'_>, head: ChangesetId, limit: usize) -> Result<Vec<ChangesetId>> {
    let mut out = Vec::new();
    let mut cursor = head;
    while out.len() < limit {
        out.push(cursor);
        match ctx.derive::<SkipList>(cursor).await?.parents.first() {
            Some((parent, _)) => cursor = *parent,
            None => break,
        }
    }
    Ok(out)
}

/// Up to `limit` changesets on the first-parent chain of `head` that changed
/// `path` (a file, or anything beneath a directory), newest first. One
/// `PathHistory` lookup at `head`, then each step is one history-node read
/// ([`scion_derived::HistoryEntry::previous`]) and no metadata read.
///
/// Scope: the path's *current* existence. The walk stops at the changeset
/// that created the path as it exists at `head`; a path absent at `head`,
/// and any earlier life of a path that was deleted and re-created, yield
/// nothing. History across deletions needs an index of deleted paths, which
/// does not exist yet.
pub async fn path_log(
    ctx: &Deriver<'_>,
    head: ChangesetId,
    path: &RepoPath,
    limit: usize,
) -> Result<Vec<ChangesetId>> {
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    let name = path.file_name();
    let history = ctx.derive::<PathHistory>(head).await?;
    let mut entry = history.lookup(ctx.objects, path).await?;
    while let Some(e) = entry {
        out.push(e.last_changed());
        if out.len() == limit {
            break;
        }
        entry = e.previous(ctx.objects, &name).await?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use scion_derived::test_util::{
        Counting, CountingMeta, Flat, REPO, arb_changes, changeset, materialize, tracked,
    };
    use scion_store::{MemoryStore, MetadataStore};

    use super::*;

    // ---- naive references -----------------------------------------------

    fn naive_diff(old: &Flat, new: &Flat) -> Vec<DiffEntry> {
        let mut out = Vec::new();
        for (path, (id, kind, size)) in old {
            let o = FileEntry {
                id: *id,
                kind: *kind,
                size: *size,
            };
            match new.get(path) {
                None => out.push(DiffEntry::Deleted {
                    path: path.clone(),
                    old: o,
                }),
                Some((nid, nkind, nsize)) => {
                    let n = FileEntry {
                        id: *nid,
                        kind: *nkind,
                        size: *nsize,
                    };
                    if o != n {
                        out.push(DiffEntry::Modified {
                            path: path.clone(),
                            old: o,
                            new: n,
                        });
                    }
                }
            }
        }
        for (path, (id, kind, size)) in new {
            if !old.contains_key(path) {
                out.push(DiffEntry::Added {
                    path: path.clone(),
                    new: FileEntry {
                        id: *id,
                        kind: *kind,
                        size: *size,
                    },
                });
            }
        }
        out.sort_by(|a, b| a.path().cmp(b.path()));
        out
    }

    fn subtree<'a>(
        flat: &'a Flat,
        path: &RepoPath,
    ) -> Vec<(&'a RepoPath, &'a (BlobId, FileType, u64))> {
        let prefix = format!("{path}/");
        flat.iter()
            .filter(|(p, _)| *p == path || p.as_str().starts_with(&prefix))
            .collect()
    }

    /// Changesets `ids[..=k]` (a first-parent chain) whose subtree at `path`
    /// differs from the previous one's, newest first, stopping where the path
    /// does not exist (see `path_log`'s documented scope).
    fn naive_path_log(
        ids: &[ChangesetId],
        flats: &[Flat],
        k: usize,
        path: &RepoPath,
    ) -> Vec<ChangesetId> {
        let empty = Flat::new();
        let mut out = Vec::new();
        for j in (0..=k).rev() {
            if subtree(&flats[j], path).is_empty() {
                break;
            }
            let before = if j == 0 { &empty } else { &flats[j - 1] };
            if subtree(&flats[j], path) != subtree(before, path) {
                out.push(ids[j]);
            }
        }
        out
    }

    // ---- tests ----------------------------------------------------------

    #[tokio::test]
    async fn diff_reports_files_not_directories_and_handles_type_changes() {
        let store = MemoryStore::new();
        let d = Deriver::new(REPO, &store, &store);
        let p = |s: &str| RepoPath::new(s).unwrap();

        let mut c = BTreeMap::new();
        c.insert(p("a/x"), tracked(b"x"));
        c.insert(p("a/y"), tracked(b"y"));
        c.insert(p("b"), tracked(b"b"));
        let c0 = store
            .put_changeset(&changeset(vec![], c, "c0"))
            .await
            .unwrap();

        // a becomes a file, b becomes a directory, a/x's content changes first.
        let mut c = BTreeMap::new();
        c.insert(p("a"), tracked(b"A"));
        c.insert(p("b/z"), tracked(b"z"));
        let c1 = store
            .put_changeset(&changeset(vec![c0], c, "c1"))
            .await
            .unwrap();

        let entries = diff(&d, Some(c0), c1).await.unwrap();
        let summary: Vec<(String, &str)> = entries
            .iter()
            .map(|e| {
                let kind = match e {
                    DiffEntry::Added { .. } => "A",
                    DiffEntry::Deleted { .. } => "D",
                    DiffEntry::Modified { .. } => "M",
                };
                (e.path().to_string(), kind)
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("a", "A"),
                ("a/x", "D"),
                ("a/y", "D"),
                ("b", "D"),
                ("b/z", "A")
            ]
            .map(|(p, k)| (p.to_string(), k))
        );

        // Against the empty tree everything is Added; identical trees diff to nothing.
        assert_eq!(diff(&d, None, c0).await.unwrap().len(), 3);
        assert_eq!(diff(&d, Some(c1), c1).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn diff_skips_equal_subtrees() {
        let store = MemoryStore::new();
        let mut c = BTreeMap::new();
        for i in 0..50 {
            for j in 0..20 {
                c.insert(
                    RepoPath::new(format!("d{i}/f{j}")).unwrap(),
                    tracked(format!("{i}{j}").as_bytes()),
                );
            }
        }
        let c0 = store
            .put_changeset(&changeset(vec![], c, "c0"))
            .await
            .unwrap();
        let mut c = BTreeMap::new();
        c.insert(RepoPath::new("d7/f3").unwrap(), tracked(b"changed"));
        let c1 = store
            .put_changeset(&changeset(vec![c0], c, "c1"))
            .await
            .unwrap();
        let d = Deriver::new(REPO, &store, &store);
        d.derive::<RootTree>(c1).await.unwrap();

        let counting = Counting::new(&store);
        let d = Deriver::new(REPO, &store, &counting);
        let entries = diff(&d, Some(c0), c1).await.unwrap();
        assert_eq!(entries.len(), 1);
        // Only the root and d7 were loaded on each side.
        assert!(
            counting.tree_gets() <= 4,
            "tree reads: {}",
            counting.tree_gets()
        );
    }

    #[tokio::test]
    async fn log_follows_first_parent_without_loading_changesets() {
        let store = MemoryStore::new();
        let mut ids = Vec::new();
        for i in 0..20 {
            let parents = ids.last().copied().into_iter().collect();
            ids.push(
                store
                    .put_changeset(&changeset(parents, BTreeMap::new(), &i.to_string()))
                    .await
                    .unwrap(),
            );
        }
        let branch = store
            .put_changeset(&changeset(vec![ids[5]], BTreeMap::new(), "b"))
            .await
            .unwrap();
        let merge = store
            .put_changeset(&changeset(vec![ids[19], branch], BTreeMap::new(), "m"))
            .await
            .unwrap();
        Deriver::new(REPO, &store, &store)
            .derive::<SkipList>(merge)
            .await
            .unwrap();

        let counting = CountingMeta::new(&store);
        let d = Deriver::new(REPO, &counting, &store);
        let got = log(&d, merge, 5).await.unwrap();
        assert_eq!(got, [merge, ids[19], ids[18], ids[17], ids[16]]);
        assert_eq!(counting.changeset_reads(), 0);
        assert_eq!(
            log(&d, ids[2], 100).await.unwrap(),
            [ids[2], ids[1], ids[0]]
        );
    }

    #[tokio::test]
    async fn path_log_reads_proportional_to_changes_of_that_path() {
        let store = MemoryStore::new();
        let p = RepoPath::new("hot/file").unwrap();
        let mut ids = Vec::new();
        let mut touched = Vec::new();
        for i in 0..200 {
            let parents = ids.last().copied().into_iter().collect();
            let mut c = BTreeMap::new();
            if i % 50 == 0 {
                c.insert(p.clone(), tracked(format!("v{i}").as_bytes()));
            } else {
                c.insert(RepoPath::new(format!("other/f{i}")).unwrap(), tracked(b"x"));
            }
            let id = store
                .put_changeset(&changeset(parents, c, &i.to_string()))
                .await
                .unwrap();
            if i % 50 == 0 {
                touched.push(id);
            }
            ids.push(id);
        }
        let head = ids[199];
        let d0 = Deriver::new(REPO, &store, &store);
        d0.derive::<PathHistory>(head).await.unwrap();
        d0.derive::<SkipList>(head).await.unwrap();

        let counting = CountingMeta::new(&store);
        let objects = Counting::new(&store);
        let d = Deriver::new(REPO, &counting, &objects);
        touched.reverse();
        assert_eq!(path_log(&d, head, &p, 100).await.unwrap(), touched);
        assert_eq!(counting.changeset_reads(), 0);
        // One pointer read at the head; a scan would read 200 entries.
        assert_eq!(counting.derived_reads(), 1);
        // A lookup at depth 2, then one node read per further change.
        assert_eq!(objects.blob_gets(), 2 + touched.len() - 1);
        assert_eq!(path_log(&d, head, &p, 2).await.unwrap(), touched[..2]);
        assert_eq!(
            path_log(&d, head, &RepoPath::new("nope").unwrap(), 10)
                .await
                .unwrap(),
            []
        );
    }

    // ---- property tests: random chains ≡ naive -------------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn diff_and_path_log_equal_naive(steps in prop::collection::vec(arb_changes(), 1..8)) {
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
                }
                let n = ids.len();
                // diff against every earlier changeset and the empty tree.
                for i in 0..n {
                    prop_assert_eq!(diff(&d, None, ids[i]).await.unwrap(), naive_diff(&Flat::new(), &flats[i]));
                    for j in 0..i {
                        prop_assert_eq!(diff(&d, Some(ids[j]), ids[i]).await.unwrap(), naive_diff(&flats[j], &flats[i]), "diff {} -> {}", j, i);
                    }
                }
                // log is the chain; path_log matches the scan for every path ever seen.
                prop_assert_eq!(log(&d, ids[n - 1], 100).await.unwrap(), ids.iter().rev().copied().collect::<Vec<_>>());
                let mut paths: Vec<RepoPath> = flats.iter().flat_map(|f| f.keys().cloned()).collect();
                paths.extend(flats.iter().flat_map(|f| f.keys().flat_map(|p| std::iter::successors(p.parent(), RepoPath::parent))));
                paths.sort();
                paths.dedup();
                for path in paths {
                    prop_assert_eq!(
                        path_log(&d, ids[n - 1], &path, 100).await.unwrap(),
                        naive_path_log(&ids, &flats, n - 1, &path),
                        "path_log {}", path
                    );
                }
                Ok(())
            })?;
        }
    }
}
