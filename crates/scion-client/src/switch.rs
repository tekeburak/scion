//! `checkout <rev>` and `update`: move a working copy to another changeset.
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
use scion_types::{ChangesetId, RepoPath};

use crate::checkout::{CheckoutError, FETCH_CONCURRENCY, File, write_file};
use crate::index::Index;
use crate::remote::{Rebased, Remote, RemoteError};
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
        size: f.size,
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
    let written: Vec<_> = stream::iter(plan.into_values().flatten().map(|f| {
        let mut remote = remote.clone();
        async move {
            let data = remote.get_blob(f.id).await?;
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
