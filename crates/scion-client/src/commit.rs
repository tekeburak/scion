//! `commit`: turn local edits into a changeset on the server.
//!
//! The new changeset records the working copy's changes relative to its
//! current head (the first and only parent): modified and added files are
//! uploaded as blobs, only those the server does not already have, and files
//! gone from disk are deleted. It becomes the
//! working copy's new anonymous head; no server ref moves, unless
//! [`commit_and_land`] lands it on `main` in the same round trip.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::path::PathBuf;

use futures_util::{StreamExt, TryStreamExt, stream};
use scion_proto::MAX_IDS;
use scion_proto::trace::{span, timed};
use scion_types::{BlobId, Changeset, ChangesetId, FileChange, Identity, RepoPath, Timestamp};

use crate::checkout::BATCHES_IN_FLIGHT;
use crate::index::{Index, IndexEntry};
use crate::remote::{Remote, RemoteChangeRequest, RemoteError};
use crate::status::{FileStatus, StatusError, read_working_file, status};
use crate::workspace::{State, Workspace, WorkspaceError};

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
    #[error("conflict markers remain; resolve them first{}", list(.0))]
    Unresolved(Vec<RepoPath>),
}

/// `paths`, one per line, indented.
fn list(paths: &[RepoPath]) -> String {
    paths.iter().fold(String::new(), |mut out, p| {
        out.push_str("\n  ");
        out.push_str(p.as_str());
        out
    })
}

/// Whether the file at `path` still has a line opening or closing a
/// conflict block: the marker alone or followed by a space or tab, so a
/// longer run of `<` or `>` in ordinary text is not one.
fn has_markers(path: &std::path::Path) -> bool {
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    data.split(|b| *b == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        b"<>".iter().any(|&c| {
            line.len() >= scion_diff3::MARKER_LEN
                && line[..scion_diff3::MARKER_LEN].iter().all(|b| *b == c)
                && matches!(line.get(scion_diff3::MARKER_LEN), None | Some(b' ' | b'\t'))
        })
    })
}

/// Read the files at `paths`, upload the blobs the server does not have yet,
/// and return their new index entries: one `HasBlobs` and at most one
/// `PutBlobs`, so at most [`MAX_IDS`] paths. Each file's metadata is taken
/// before its read, so an edit racing the commit leaves a stale mtime and is
/// rehashed by the next `status`.
async fn upload(
    ws: &Workspace,
    remote: &Remote,
    repo: &str,
    paths: &[RepoPath],
) -> Result<Vec<(RepoPath, IndexEntry)>, CommitError> {
    let mut read = Vec::with_capacity(paths.len());
    for path in paths {
        let file = ws.root().join(path.as_str());
        let io_err = |e| CommitError::Io(file.clone(), e);
        let meta = std::fs::symlink_metadata(&file).map_err(io_err)?;
        let (kind, data) = read_working_file(&file).map_err(io_err)?;
        let entry = IndexEntry::new(BlobId::hash(&data), kind, &meta);
        read.push((path.clone(), entry, data));
    }
    let ids: Vec<BlobId> = read
        .iter()
        .map(|(_, e, _)| e.id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let present = remote.clone().has_blobs(repo, &ids).await?;
    let mut missing: HashSet<BlobId> = ids
        .into_iter()
        .zip(present)
        .filter_map(|(id, present)| (!present).then_some(id))
        .collect();
    let mut blobs = Vec::new();
    let mut entries = Vec::with_capacity(read.len());
    for (path, entry, data) in read {
        if missing.remove(&entry.id) {
            blobs.push(data);
        }
        entries.push((path, entry));
    }
    if !blobs.is_empty() {
        remote.clone().put_blobs(repo, blobs).await?;
    }
    Ok(entries)
}

/// Commit every modified, added and deleted file in `ws` as a child of its
/// current head, then make the result the current head. Untracked files that
/// were not added are left out. A file `update --conflicts` marked is
/// refused while it still holds a conflict block.
pub async fn commit(
    ws: &Workspace,
    remote: &Remote,
    author: Identity,
    date: Timestamp,
    message: &str,
) -> Result<ChangesetId, CommitError> {
    let pending = prepare(ws, remote, author, date, message).await?;
    let id = timed("create", remote.clone().create_changeset(&pending.cs)).await?;
    pending.record(ws, id)?;
    Ok(id)
}

/// [`commit`], and ask the server to land the new head on `main` in the same
/// round trip (`CommitAndLand`). Returns the change request, finished unless
/// the server's longest wait ran out first; its `head` is the new current
/// head.
pub async fn commit_and_land(
    ws: &Workspace,
    remote: &Remote,
    author: Identity,
    date: Timestamp,
    message: &str,
) -> Result<RemoteChangeRequest, CommitError> {
    let pending = prepare(ws, remote, author, date, message).await?;
    let cr = timed("land", remote.clone().commit_and_land(&pending.cs)).await?;
    pending.record(ws, cr.head)?;
    Ok(cr)
}

/// A changeset whose blobs are uploaded, and what the working copy records
/// once it is created.
struct Pending {
    cs: Changeset,
    state: State,
    index: Index,
    deleted: Vec<RepoPath>,
    uploaded: Vec<(RepoPath, IndexEntry)>,
}

/// Upload what `commit` commits and build its changeset.
async fn prepare(
    ws: &Workspace,
    remote: &Remote,
    author: Identity,
    date: Timestamp,
    message: &str,
) -> Result<Pending, CommitError> {
    let config = ws.read_config()?;
    let state = ws.read_state()?;
    let index = ws.read_index()?;
    let unresolved: Vec<RepoPath> = ws
        .read_conflicts()?
        .into_iter()
        .filter(|p| has_markers(&ws.root().join(p.as_str())))
        .collect();
    if !unresolved.is_empty() {
        return Err(CommitError::Unresolved(unresolved));
    }
    let changes = status(ws.root(), &index, &ws.read_added()?)?;
    let put = span("put");

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

    let repo = config.repo.as_str();
    let uploaded: Vec<(RepoPath, IndexEntry)> = stream::iter(
        upload_paths
            .chunks(MAX_IDS)
            .map(|paths| upload(ws, remote, repo, paths)),
    )
    .buffer_unordered(BATCHES_IN_FLIGHT)
    .try_concat()
    .await?;
    drop(put);

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
    Ok(Pending {
        cs,
        state,
        index,
        deleted,
        uploaded,
    })
}

impl Pending {
    /// Make `id`, the created changeset, the current head.
    fn record(mut self, ws: &Workspace, id: ChangesetId) -> Result<(), CommitError> {
        // Index before state: if we stop in between, status still compares
        // the files with what was committed.
        for path in &self.deleted {
            self.index.entries.remove(path);
        }
        self.index.entries.extend(self.uploaded);
        ws.write_index(&mut self.index)?;
        self.state.current_head = id;
        ws.write_state(&self.state)?;
        ws.write_added(&BTreeSet::new())?;
        ws.write_conflicts(&BTreeSet::new())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_marker_lines_count() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        for (text, marked) in [
            (&b"<<<<<<< ours\nA\n=======\nB\n>>>>>>> theirs\n"[..], true),
            (b"x\n<<<<<<<\n", true),
            (b"x\r\n>>>>>>> theirs\r\n", true),
            (b"<<<<<<<< eight\n=======\nsetext heading\n", false),
            (b"a <<<<<<< b\n", false),
        ] {
            std::fs::write(&file, text).unwrap();
            assert_eq!(
                has_markers(&file),
                marked,
                "{}",
                String::from_utf8_lossy(text)
            );
        }
        assert!(!has_markers(&dir.path().join("gone")));
    }
}
