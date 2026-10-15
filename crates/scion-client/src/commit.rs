//! `commit`: turn local edits into a changeset on the server.
//!
//! The new changeset records the working copy's changes relative to its
//! current head (the first and only parent): modified and added files are
//! uploaded as blobs, files gone from disk are deleted. It becomes the
//! working copy's new anonymous head; no server ref moves.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;

use futures_util::{StreamExt, TryStreamExt, stream};
use scion_types::{Changeset, ChangesetId, FileChange, Identity, RepoPath, Timestamp};

use crate::checkout::{FETCH_CONCURRENCY, mtime_ns};
use crate::index::IndexEntry;
use crate::remote::{Remote, RemoteError};
use crate::status::{FileStatus, StatusError, read_working_file, status};
use crate::workspace::{Workspace, WorkspaceError};

/// Failure during commit.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Status(#[from] StatusError),
    #[error("{0}: {1}")]
    Io(PathBuf, #[source] io::Error),
    #[error("nothing to commit")]
    NothingToCommit,
}

/// Read the file at `path`, upload it, and return its new index entry. The
/// metadata is taken before the read, so an edit racing the commit leaves a
/// stale mtime and is rehashed by the next `status`.
async fn upload(
    ws: &Workspace,
    remote: &Remote,
    path: &RepoPath,
) -> Result<IndexEntry, CommitError> {
    let file = ws.root().join(path.as_str());
    let io_err = |e| CommitError::Io(file.clone(), e);
    let meta = std::fs::symlink_metadata(&file).map_err(io_err)?;
    let (kind, data) = read_working_file(&file).map_err(io_err)?;
    let size = data.len() as u64;
    let id = remote.clone().put_blob(data).await?;
    Ok(IndexEntry {
        id,
        kind,
        size,
        mtime_ns: mtime_ns(&meta),
    })
}

/// Commit every modified, added and deleted file in `ws` as a child of its
/// current head, then make the result the current head. Untracked files that
/// were not added are left out.
pub async fn commit(
    ws: &Workspace,
    remote: &Remote,
    author: Identity,
    date: Timestamp,
    message: &str,
) -> Result<ChangesetId, CommitError> {
    let config = ws.read_config()?;
    let mut state = ws.read_state()?;
    let mut index = ws.read_index()?;
    let changes = status(ws.root(), &index, &ws.read_added()?)?;

    let mut deleted = Vec::new();
    let mut upload_paths = Vec::new();
    for (path, st) in changes {
        match st {
            FileStatus::Modified | FileStatus::Added => upload_paths.push(path),
            FileStatus::Deleted => deleted.push(path),
            FileStatus::Untracked => {}
        }
    }
    if deleted.is_empty() && upload_paths.is_empty() {
        return Err(CommitError::NothingToCommit);
    }

    let uploaded: Vec<(RepoPath, IndexEntry)> =
        stream::iter(upload_paths.into_iter().map(|path| async move {
            let entry = upload(ws, remote, &path).await?;
            Ok::<_, CommitError>((path, entry))
        }))
        .buffer_unordered(FETCH_CONCURRENCY)
        .try_collect()
        .await?;

    let mut file_changes = BTreeMap::new();
    for path in &deleted {
        file_changes.insert(path.clone(), FileChange::Deleted);
    }
    for (path, e) in &uploaded {
        file_changes.insert(
            path.clone(),
            FileChange::Tracked {
                content: e.id,
                kind: e.kind,
                size: e.size,
                copy_from: None,
            },
        );
    }
    let cs = Changeset {
        repo: config.repo,
        parents: vec![state.current_head],
        author,
        author_date: date,
        committer: None,
        committer_date: None,
        message: message.to_owned(),
        file_changes,
        extra: BTreeMap::new(),
    };
    let id = remote.clone().create_changeset(&cs).await?;

    // Index before state: if we stop in between, status still compares the
    // files with what was committed.
    for path in &deleted {
        index.entries.remove(path);
    }
    index.entries.extend(uploaded);
    ws.write_index(&index)?;
    state.current_head = id;
    ws.write_state(&state)?;
    ws.write_added(&BTreeSet::new())?;
    Ok(id)
}
