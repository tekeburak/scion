//! `checkout <rev>`, `update`, `merge` and `revert`: move a working copy to
//! another changeset.
//!
//! `update --conflicts` is the one move that leaves local edits behind: when
//! the server cannot replay the head, the working copy goes to the target
//! with the head's changes on top, conflicting files holding three-way
//! markers, for the next commit to land as one resolved changeset.
//!
//! Only what differs between the two changesets is touched: the server's
//! `Diff` names the files, each is removed and, if the target has it,
//! written again. Files outside the sparse profile are never fetched.
//! Everything that could stop the move (local edits, an untracked file in
//! the way, a rebase conflict) is checked before the first file changes.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use futures_util::{StreamExt, TryStreamExt, stream};
use scion_types::{BlobId, ChangesetId, FileType, Identity, RepoPath, Timestamp};

use crate::checkout::{CheckoutError, FETCH_CONCURRENCY, File, write_file};
use crate::index::Index;
use crate::remote::{FileEntry, Merged, Rebased, Remote, RemoteError, Reverted};
use crate::status::{FileStatus, StatusError, status};
use crate::workspace::{Workspace, WorkspaceError};

/// `paths`, one per line, indented.
fn list(paths: &[RepoPath]) -> String {
    paths.iter().fold(String::new(), |mut out, p| {
        out.push_str("\n  ");
        out.push_str(p.as_str());
        out
    })
}

/// Failure moving the working copy. Only `Remote` and `Checkout` can leave
/// it half moved, and only once files are being written.
#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Status(#[from] StatusError),
    #[error(transparent)]
    Checkout(#[from] CheckoutError),
    #[error("local changes would be lost; commit them first{}", list(.0))]
    Dirty(Vec<RepoPath>),
    #[error("untracked files would be overwritten; move them away first{}", list(.0))]
    Untracked(Vec<RepoPath>),
    #[error("{changeset} conflicts with {onto}; nothing was changed{}", list(.paths))]
    Conflict {
        changeset: ChangesetId,
        onto: ChangesetId,
        paths: Vec<RepoPath>,
    },
    #[error(
        "cannot revert {changeset}: changed again since; nothing was changed{}",
        list(.paths)
    )]
    ChangedSince {
        changeset: ChangesetId,
        paths: Vec<RepoPath>,
    },
}

/// Remove `rel` under `root` if it is a file or symlink, then every parent
/// directory that is left empty.
fn remove(root: &Path, rel: &RepoPath) -> Result<(), SwitchError> {
    let path = root.join(rel.as_str());
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if !meta.is_dir() => {
            std::fs::remove_file(&path).map_err(|e| CheckoutError::Io(path.clone(), e))?;
        }
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(CheckoutError::Io(path, e).into()),
    }
    let mut dir = path.parent();
    while let Some(d) = dir.filter(|d| *d != root) {
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

/// Move the files of `ws` from changeset `from` to `to`, updating `index`.
/// `restore` names local edits to throw away (files the index has, at
/// their indexed version); with `overwrite`, untracked files in the way are
/// replaced instead of refused.
async fn apply(
    ws: &Workspace,
    remote: &Remote,
    index: &mut Index,
    (from, to): (ChangesetId, ChangesetId),
    restore: &[RepoPath],
    overwrite: bool,
) -> Result<(), SwitchError> {
    let repo = ws.read_config()?.repo;
    let sparse = ws.read_sparse()?;
    let file = |path: &RepoPath, f: crate::FileEntry| File {
        path: path.clone(),
        id: f.id,
        kind: f.kind,
    };
    let mut plan: BTreeMap<RepoPath, Option<File>> = BTreeMap::new();
    if from != to {
        for e in remote.clone().diff(&repo, Some(from), to).await? {
            if sparse.includes(&e.path) {
                let new = e.new.map(|f| file(&e.path, f));
                plan.insert(e.path, new);
            }
        }
    }
    // A path the move does not touch is the same in `to` as in the index.
    for path in restore {
        if let (false, Some(e)) = (plan.contains_key(path), index.entries.get(path)) {
            let entry = crate::FileEntry {
                id: e.id,
                kind: e.kind,
                size: e.size,
            };
            plan.insert(path.clone(), Some(file(path, entry)));
        }
    }

    let root = ws.root();
    let in_the_way: Vec<RepoPath> = plan
        .iter()
        .filter(|(path, new)| {
            new.is_some()
                && !index.entries.contains_key(*path)
                && std::fs::symlink_metadata(root.join(path.as_str())).is_ok()
        })
        .map(|(path, _)| path.clone())
        .collect();
    if !overwrite && !in_the_way.is_empty() {
        return Err(SwitchError::Untracked(in_the_way));
    }

    // Removals first, so a file can replace a directory and the other way
    // round.
    for path in plan.keys() {
        remove(root, path)?;
        index.entries.remove(path);
    }
    let repo = &repo;
    let written: Vec<_> = stream::iter(plan.into_values().flatten().map(|f| {
        let mut remote = remote.clone();
        async move {
            let data = remote.get_blob(repo, f.id).await?;
            let entry = write_file(root, &f, data).await?;
            Ok::<_, SwitchError>((f.path, entry))
        }
    }))
    .buffer_unordered(FETCH_CONCURRENCY)
    .try_collect()
    .await?;
    index.entries.extend(written);
    ws.write_index(index)?;
    Ok(())
}

/// The paths with local edits: modified, deleted or added.
fn local_edits(ws: &Workspace, index: &Index) -> Result<Vec<RepoPath>, SwitchError> {
    Ok(status(ws.root(), index, &ws.read_added()?)?
        .into_iter()
        .filter(|(_, st)| *st != FileStatus::Untracked)
        .map(|(path, _)| path)
        .collect())
}

/// Move the working copy to changeset `to`. Local edits are refused unless
/// `force`, which throws them away (added files stay on disk, untracked)
/// and replaces untracked files in the way.
pub async fn switch(
    ws: &Workspace,
    remote: &Remote,
    to: ChangesetId,
    force: bool,
) -> Result<(), SwitchError> {
    let mut state = ws.read_state()?;
    let mut index = ws.read_index()?;
    let edits = local_edits(ws, &index)?;
    if !force && !edits.is_empty() {
        return Err(SwitchError::Dirty(edits));
    }
    apply(
        ws,
        remote,
        &mut index,
        (state.current_head, to),
        &edits,
        force,
    )
    .await?;
    ws.write_added(&BTreeSet::new())?;
    ws.write_conflicts(&BTreeSet::new())?;
    state.current_head = to;
    ws.write_state(&state)?;
    Ok(())
}

/// What [`update`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Updated {
    /// The working copy's new head.
    pub head: ChangesetId,
    /// Changesets the server created by replaying the old head's.
    pub created: u32,
}

/// Rebase the working copy's head onto `onto` on the server and move the
/// working copy to the result, after which landing it is a fast-forward of
/// `onto`. Local edits are refused; on a conflict nothing changes.
pub async fn update(
    ws: &Workspace,
    remote: &Remote,
    onto: ChangesetId,
) -> Result<Updated, SwitchError> {
    let repo = ws.read_config()?.repo;
    let mut state = ws.read_state()?;
    let mut index = ws.read_index()?;
    let edits = local_edits(ws, &index)?;
    if !edits.is_empty() {
        return Err(SwitchError::Dirty(edits));
    }
    let (head, created) = match remote
        .clone()
        .rebase(&repo, state.current_head, onto)
        .await?
    {
        Rebased::Clean { head, created } => (head, created),
        Rebased::Conflict { changeset, paths } => {
            return Err(SwitchError::Conflict {
                changeset,
                onto,
                paths,
            });
        }
    };
    apply(
        ws,
        remote,
        &mut index,
        (state.current_head, head),
        &[],
        false,
    )
    .await?;
    state.current_head = head;
    state.base = onto;
    ws.write_state(&state)?;
    Ok(Updated { head, created })
}

/// Merge `theirs` into the working copy's head on the server, as a changeset
/// by `author` at `date`, and move the working copy to it; returns the new
/// head, which is the old one if it already contained `theirs`. Local edits
/// are refused; on a conflict nothing changes.
pub async fn merge(
    ws: &Workspace,
    remote: &Remote,
    theirs: ChangesetId,
    (author, date): (&Identity, Timestamp),
    message: &str,
) -> Result<ChangesetId, SwitchError> {
    let repo = ws.read_config()?.repo;
    let mut state = ws.read_state()?;
    let mut index = ws.read_index()?;
    let edits = local_edits(ws, &index)?;
    if !edits.is_empty() {
        return Err(SwitchError::Dirty(edits));
    }
    let ours = state.current_head;
    let head = match remote
        .clone()
        .merge(&repo, (ours, theirs), (author, date), message)
        .await?
    {
        Merged::Clean { merged, .. } => merged,
        Merged::Conflict { paths } => {
            return Err(SwitchError::Conflict {
                changeset: theirs,
                onto: ours,
                paths,
            });
        }
    };
    apply(ws, remote, &mut index, (ours, head), &[], false).await?;
    state.current_head = head;
    ws.write_state(&state)?;
    Ok(head)
}

/// What [`update_with_markers`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marked {
    /// The head was rebased cleanly, as by [`update`].
    Updated(Updated),
    /// The working copy is at the target, with the old head's changes as
    /// local edits; `conflicts` (sorted) hold conflict markers, or the side
    /// that kept a file the other deleted. The next commit lands them on
    /// the target as one changeset.
    Conflicted { conflicts: Vec<RepoPath> },
}

/// What a marked update does to one path.
enum Write {
    Remove,
    File(FileType, Vec<u8>),
}

/// Whether `a` is a proper ancestor directory of `b`.
fn is_ancestor_path(a: &RepoPath, b: &RepoPath) -> bool {
    b.as_str()
        .strip_prefix(a.as_str())
        .is_some_and(|rest| rest.starts_with('/'))
}

/// As [`update`], but a conflict does not refuse: the working copy moves to
/// `onto` and gets every change the head made since its merge base with
/// `onto`, merged three-way per file with `onto`'s, conflict markers (from
/// `scion-diff3`) in each file both changed on the same lines. `onto` is
/// recorded as the head, and the conflicting paths in `.scn/conflicts`,
/// which `commit` checks for markers. A change outside the sparse profile,
/// or a file on one side where the other has files beneath it, still
/// refuses and changes nothing.
pub async fn update_with_markers(
    ws: &Workspace,
    remote: &Remote,
    onto: ChangesetId,
) -> Result<Marked, SwitchError> {
    let refused = match update(ws, remote, onto).await {
        Ok(done) => return Ok(Marked::Updated(done)),
        Err(e @ SwitchError::Conflict { .. }) => e,
        Err(e) => return Err(e),
    };
    let repo = ws.read_config()?.repo;
    let sparse = ws.read_sparse()?;
    let mut state = ws.read_state()?;
    let head = state.current_head;
    let mut r = remote.clone();
    let base = r.merge_base(&repo, head, onto).await?;
    let mut other = r.clone();
    let (theirs, ours) =
        tokio::try_join!(r.diff(&repo, base, head), other.diff(&repo, base, onto))?;
    let ours: BTreeMap<RepoPath, Option<FileEntry>> =
        ours.into_iter().map(|e| (e.path, e.new)).collect();
    let no_place = theirs.iter().any(|t| {
        !sparse.includes(&t.path)
            || t.new.is_some()
                && ours.iter().any(|(p, new)| {
                    new.is_some() && (is_ancestor_path(p, &t.path) || is_ancestor_path(&t.path, p))
                })
    });
    if no_place {
        return Err(refused);
    }

    // Decide every path before the first file changes.
    let mut plan = Vec::new();
    let mut conflicts = BTreeSet::new();
    for t in theirs {
        let write = match (ours.get(&t.path), t.new) {
            (None, None) => Some(Write::Remove),
            (Some(None), None) => None,
            (None, Some(new)) => Some(Write::File(new.kind, r.get_blob(&repo, new.id).await?)),
            // Deleted on one side, changed on the other: the changed file
            // stays for the resolver to keep or delete.
            (Some(None), Some(new)) => {
                conflicts.insert(t.path.clone());
                Some(Write::File(new.kind, r.get_blob(&repo, new.id).await?))
            }
            (Some(Some(_)), None) => {
                conflicts.insert(t.path.clone());
                None
            }
            (Some(Some(o)), Some(new)) if *o == new => None,
            (Some(Some(o)), Some(new)) => {
                let base_kind = t.old.map(|b| b.kind);
                let kind = if o.kind == new.kind || Some(new.kind) == base_kind {
                    o.kind
                } else if Some(o.kind) == base_kind {
                    new.kind
                } else {
                    conflicts.insert(t.path.clone());
                    continue;
                };
                let base_bytes = match t.old {
                    Some(b) => r.get_blob(&repo, b.id).await?,
                    None => Vec::new(),
                };
                let (o_bytes, t_bytes) = (
                    r.get_blob(&repo, o.id).await?,
                    r.get_blob(&repo, new.id).await?,
                );
                let merged = scion_diff3::merge(&base_bytes, &o_bytes, &t_bytes);
                if !merged.is_clean() {
                    conflicts.insert(t.path.clone());
                }
                Some(Write::File(kind, merged.into_content()))
            }
        };
        if let Some(write) = write {
            plan.push((t.path, write));
        }
    }

    let mut index = ws.read_index()?;
    apply(ws, remote, &mut index, (head, onto), &[], false).await?;
    // The index stays at `onto`, so everything written is a local edit.
    let root = ws.root();
    let mut added = BTreeSet::new();
    for (path, write) in plan {
        remove(root, &path)?;
        if let Write::File(kind, data) = write {
            let file = File {
                path: path.clone(),
                id: BlobId::hash(&data),
                kind,
            };
            write_file(root, &file, data).await?;
            if !index.entries.contains_key(&path) {
                added.insert(path);
            }
        }
    }
    ws.write_added(&added)?;
    ws.write_conflicts(&conflicts)?;
    state.current_head = onto;
    state.base = onto;
    ws.write_state(&state)?;
    Ok(Marked::Conflicted {
        conflicts: conflicts.into_iter().collect(),
    })
}

/// Undo `target` on the working copy's head on the server, as a changeset by
/// `author` at `date`, and move the working copy to it; returns the new
/// head. Local edits are refused; when a path `target` changed was changed
/// again since, nothing changes.
pub async fn revert(
    ws: &Workspace,
    remote: &Remote,
    target: ChangesetId,
    (author, date): (&Identity, Timestamp),
    message: &str,
) -> Result<ChangesetId, SwitchError> {
    let repo = ws.read_config()?.repo;
    let mut state = ws.read_state()?;
    let mut index = ws.read_index()?;
    let edits = local_edits(ws, &index)?;
    if !edits.is_empty() {
        return Err(SwitchError::Dirty(edits));
    }
    let old = state.current_head;
    let head = match remote
        .clone()
        .revert(&repo, (old, target), (author, date), message)
        .await?
    {
        Reverted::Clean { reverted } => reverted,
        Reverted::ChangedSince { paths } => {
            return Err(SwitchError::ChangedSince {
                changeset: target,
                paths,
            });
        }
    };
    apply(ws, remote, &mut index, (old, head), &[], false).await?;
    state.current_head = head;
    ws.write_state(&state)?;
    Ok(head)
}
