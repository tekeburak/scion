//! Clone and checkout: resolve a root tree into files on disk.
//!
//! Trees are fetched level by level, only where the sparse profile reaches;
//! then every included blob is fetched and written. Each request names one
//! immutable object, so the exchange is resumable and any tier could answer.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use futures_util::{StreamExt, TryStreamExt, stream};
use scion_types::{BlobId, FileType, RepoPath, TreeEntry, TreeId};

use crate::index::{Index, IndexEntry};
use crate::remote::{Remote, RemoteError};
use crate::sparse::SparseProfile;
use crate::workspace::{Config, State, Workspace, WorkspaceError};

/// The server ref a clone starts from.
pub const MAIN_REF: &str = "main";

/// Most object requests in flight at once during a checkout or commit.
pub(crate) const FETCH_CONCURRENCY: usize = 64;

/// Failure during clone or checkout.
#[derive(Debug, thiserror::Error)]
pub enum CheckoutError {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("{0}: {1}")]
    Io(PathBuf, #[source] io::Error),
    #[error("{0} exists and is not empty")]
    NotEmpty(PathBuf),
}

/// A file the checkout will write.
struct File {
    path: RepoPath,
    id: BlobId,
    kind: FileType,
    size: u64,
}

/// Every file under `root` that `sparse` includes, fetching only the trees
/// it reaches.
async fn list_files(
    remote: &Remote,
    root: TreeId,
    sparse: &SparseProfile,
) -> Result<Vec<File>, RemoteError> {
    let mut files = Vec::new();
    let mut level: Vec<(Option<RepoPath>, TreeId)> = vec![(None, root)];
    while !level.is_empty() {
        let trees: Vec<_> = stream::iter(level.into_iter().map(|(dir, id)| {
            let mut remote = remote.clone();
            async move { Ok::<_, RemoteError>((dir, remote.get_tree(id).await?)) }
        }))
        .buffer_unordered(FETCH_CONCURRENCY)
        .try_collect()
        .await?;
        let mut next = Vec::new();
        for (dir, tree) in trees {
            for (name, entry) in tree.entries {
                let path = match &dir {
                    None => RepoPath::new(name.as_str()).expect("a component is a path"),
                    Some(dir) => dir.join(&name),
                };
                match entry {
                    TreeEntry::Tree { id } if sparse.descends_into(&path) => {
                        next.push((Some(path), id));
                    }
                    TreeEntry::Blob { id, kind, size } if sparse.includes(&path) => {
                        files.push(File {
                            path,
                            id,
                            kind,
                            size,
                        });
                    }
                    TreeEntry::Tree { .. } | TreeEntry::Blob { .. } => {}
                }
            }
        }
        level = next;
    }
    Ok(files)
}

pub(crate) fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

/// Write one file and return its index entry.
async fn write_file(root: &Path, file: &File, data: Vec<u8>) -> Result<IndexEntry, CheckoutError> {
    let dest = root.join(file.path.as_str());
    let io_err = |e| CheckoutError::Io(dest.clone(), e);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(io_err)?;
    }
    match file.kind {
        FileType::Symlink => {
            let target = String::from_utf8_lossy(&data).into_owned();
            tokio::fs::symlink(target, &dest).await.map_err(io_err)?;
        }
        FileType::Regular | FileType::Executable => {
            tokio::fs::write(&dest, &data).await.map_err(io_err)?;
            if file.kind == FileType::Executable {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
                    .await
                    .map_err(io_err)?;
            }
        }
    }
    let meta = tokio::fs::symlink_metadata(&dest).await.map_err(io_err)?;
    Ok(IndexEntry {
        id: file.id,
        kind: file.kind,
        size: file.size,
        mtime_ns: mtime_ns(&meta),
    })
}

/// Materialize the part of tree `tree` that `sparse` includes under `root`,
/// which must not already hold any of those files. Returns the new index.
pub async fn checkout(
    remote: &Remote,
    root: &Path,
    tree: TreeId,
    sparse: &SparseProfile,
) -> Result<Index, CheckoutError> {
    let files = list_files(remote, tree, sparse).await?;
    let entries: BTreeMap<RepoPath, IndexEntry> = stream::iter(files.into_iter().map(|file| {
        let mut remote = remote.clone();
        async move {
            let data = remote.get_blob(file.id).await?;
            let entry = write_file(root, &file, data).await?;
            Ok::<_, CheckoutError>((file.path, entry))
        }
    }))
    .buffer_unordered(FETCH_CONCURRENCY)
    .try_collect()
    .await?;
    Ok(Index { entries })
}

/// Clone `repo` from `server` into `dest` at the current `main`,
/// materializing only what `sparse` includes. `dest` must be empty or absent.
pub async fn clone(
    server: &str,
    repo: &str,
    dest: &Path,
    sparse: SparseProfile,
) -> Result<Workspace, CheckoutError> {
    if let Ok(mut entries) = std::fs::read_dir(dest) {
        if entries.next().is_some() {
            return Err(CheckoutError::NotEmpty(dest.to_owned()));
        }
    }
    let mut remote = Remote::connect(server).await?;
    let main = remote.get_ref(repo, MAIN_REF).await?;
    let ws = Workspace::init(
        dest,
        &Config {
            server: server.to_owned(),
            repo: repo.to_owned(),
        },
    )?;
    ws.write_sparse(&sparse)?;
    let index = checkout(&remote, dest, main.root_tree, &sparse).await?;
    ws.write_index(&index)?;
    ws.write_state(&State {
        current_head: main.target,
        base: main.target,
        heads: BTreeMap::new(),
    })?;
    Ok(ws)
}
