//! Clone and checkout: resolve a root tree into files on disk.
//!
//! Trees are fetched level by level, only where the sparse profile reaches,
//! and a directory it includes whole comes with every tree beneath it; then
//! every included blob is fetched and written, [`MAX_IDS`] per request. Each
//! request names immutable objects, so the exchange is resumable and any
//! tier could answer.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use futures_util::{StreamExt, TryStreamExt, stream};
use scion_proto::MAX_IDS;
use scion_proto::trace::{span, timed};
use scion_types::{BlobId, FileType, RepoPath, Tree, TreeEntry, TreeId};

use crate::index::{Index, IndexEntry};
use crate::remote::{Fetched, Remote, RemoteError};
use crate::sparse::SparseProfile;
use crate::workspace::{Config, State, Workspace, WorkspaceError};

/// The server ref a clone starts from.
pub const MAIN_REF: &str = "main";

/// Most single-object requests in flight at once during an update.
pub(crate) const FETCH_CONCURRENCY: usize = 64;

/// Most batched object requests in flight at once during a checkout or
/// commit.
pub(crate) const BATCHES_IN_FLIGHT: usize = 8;

/// Most files of one batch being written at once.
const WRITES_PER_BATCH: usize = 16;

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
pub(crate) struct File {
    pub(crate) path: RepoPath,
    pub(crate) id: BlobId,
    pub(crate) kind: FileType,
    pub(crate) size: u64,
}

/// Fetch the trees `wanted` (each id with whether every tree beneath it is
/// wanted too) into `trees`, [`MAX_IDS`] ids per request.
async fn fetch_trees(
    remote: &Remote,
    repo: &str,
    wanted: Vec<(TreeId, bool)>,
    trees: &mut HashMap<TreeId, Tree>,
) -> Result<(), RemoteError> {
    let replies: Vec<Vec<Fetched>> = stream::iter(wanted.chunks(MAX_IDS).map(|chunk| {
        let mut remote = remote.clone();
        let (whole, plain): (Vec<_>, Vec<_>) = chunk.iter().partition(|(_, whole)| *whole);
        let ids = |v: Vec<&(TreeId, bool)>| v.into_iter().map(|(id, _)| *id).collect::<Vec<_>>();
        let (plain, whole) = (ids(plain), ids(whole));
        async move {
            remote
                .get_objects(repo, &plain, &whole, &[])
                .await?
                .try_collect()
                .await
        }
    }))
    .buffer_unordered(BATCHES_IN_FLIGHT)
    .try_collect()
    .await?;
    for fetched in replies.into_iter().flatten() {
        if let Fetched::Tree(id, tree) = fetched {
            trees.insert(id, tree);
        }
    }
    Ok(())
}

/// Every file under `root` that `sparse` includes, fetching only the trees
/// it reaches. A directory the profile includes whole comes with every tree
/// beneath it in one request; one on the way to a listed path comes alone.
async fn list_files(
    remote: &Remote,
    repo: &str,
    root: TreeId,
    sparse: &SparseProfile,
) -> Result<Vec<File>, RemoteError> {
    let mut trees = HashMap::new();
    let mut files = Vec::new();
    let mut level: Vec<(Option<RepoPath>, TreeId)> = vec![(None, root)];
    while !level.is_empty() {
        let mut wanted: HashMap<TreeId, bool> = HashMap::new();
        for (dir, id) in &level {
            if !trees.contains_key(id) {
                let whole = dir
                    .as_ref()
                    .map_or(sparse.paths().is_empty(), |d| sparse.includes(d));
                *wanted.entry(*id).or_default() |= whole;
            }
        }
        if !wanted.is_empty() {
            fetch_trees(remote, repo, wanted.into_iter().collect(), &mut trees).await?;
        }
        let mut next = Vec::new();
        for (dir, id) in level {
            let tree = trees
                .get(&id)
                .ok_or(RemoteError::Malformed("reply: a tree is missing"))?;
            for (name, entry) in &tree.entries {
                let path = match &dir {
                    None => RepoPath::new(name.as_str()).expect("a component is a path"),
                    Some(dir) => dir.join(name),
                };
                match *entry {
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
pub(crate) async fn write_file(
    root: &Path,
    file: &File,
    data: Vec<u8>,
) -> Result<IndexEntry, CheckoutError> {
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

/// Materialize the part of `repo`'s tree `tree` that `sparse` includes under
/// `root`, which must not already hold any of those files. Returns the new
/// index.
pub async fn checkout(
    remote: &Remote,
    repo: &str,
    root: &Path,
    tree: TreeId,
    sparse: &SparseProfile,
) -> Result<Index, CheckoutError> {
    let files = timed("trees", list_files(remote, repo, tree, sparse)).await?;
    let _blobs = span("blobs");
    let mut by_id: HashMap<BlobId, Vec<File>> = HashMap::new();
    for file in files {
        by_id.entry(file.id).or_default().push(file);
    }
    let ids: Vec<BlobId> = by_id.keys().copied().collect();
    let by_id = &by_id;
    let written: Vec<Vec<(RepoPath, IndexEntry)>> =
        stream::iter(ids.chunks(MAX_IDS).map(|chunk| {
            let mut remote = remote.clone();
            async move {
                remote
                    .get_objects(repo, &[], &[], chunk)
                    .await?
                    .map_err(CheckoutError::from)
                    .map_ok(|fetched| async move {
                        let Fetched::Blob(id, mut data) = fetched else {
                            return Err(RemoteError::Malformed("reply: a tree for a blob").into());
                        };
                        let files = by_id.get(&id).map_or(&[][..], Vec::as_slice);
                        let mut entries = Vec::new();
                        for (i, file) in files.iter().enumerate() {
                            let data = if i + 1 == files.len() {
                                std::mem::take(&mut data)
                            } else {
                                data.clone()
                            };
                            entries.push((file.path.clone(), write_file(root, file, data).await?));
                        }
                        Ok(entries)
                    })
                    .try_buffer_unordered(WRITES_PER_BATCH)
                    .try_concat()
                    .await
            }
        }))
        .buffer_unordered(BATCHES_IN_FLIGHT)
        .try_collect()
        .await?;
    let entries: BTreeMap<RepoPath, IndexEntry> = written.into_iter().flatten().collect();
    if entries.len() != by_id.values().map(Vec::len).sum::<usize>() {
        return Err(RemoteError::Malformed("reply: a blob is missing").into());
    }
    Ok(Index { entries })
}

/// Clone `repo` from `server`, connected as `remote`, into `dest` at the
/// current `main`, materializing only what `sparse` includes. `dest` must be
/// empty or absent.
pub async fn clone(
    mut remote: Remote,
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
    let main = remote.get_ref(repo, MAIN_REF).await?;
    let ws = Workspace::init(
        dest,
        &Config {
            server: server.to_owned(),
            repo: repo.to_owned(),
        },
    )?;
    ws.write_sparse(&sparse)?;
    let index = checkout(&remote, repo, dest, main.root_tree, &sparse).await?;
    ws.write_index(&index)?;
    ws.write_state(&State {
        current_head: main.target,
        base: main.target,
        heads: BTreeMap::new(),
    })?;
    Ok(ws)
}
