//! `ControlService`: thin wrappers over `scion-core` and `scion-derived`.
//! Requests come from clients, so ids, paths and limits are validated here.
//! `CreateChangeset` derives what clone, log and diff need eagerly; anything
//! else is derived on first request.

use std::sync::Arc;

use scion_derived::{Blame, DeriveError, Deriver, PathHistory, RootTree, SkipList};
use scion_proto::v1::control_service_server::ControlService;
use scion_proto::v1::{
    BlameRequest, BlameResponse, BlameRun, CreateChangesetRequest, CreateChangesetResponse,
    DiffEntry, DiffRequest, DiffResponse, FileEntry, FileType, GetRefRequest, GetRefResponse,
    LogRequest, LogResponse, MergeBaseRequest, MergeBaseResponse, MoveRefRequest, MoveRefResponse,
    PathLogRequest,
};
use scion_store::{MetadataStore, ObjectStore, StoreError, is_allowed_ref_name};
use scion_types::{BlobId, Changeset, ChangesetId, Decode, Encode, FileChange, RepoPath, TreeId};
use tonic::{Request, Response, Status};

use crate::staged::Staged;
use crate::store_status;

/// Most changesets a single `Log` or `PathLog` call returns; larger limits are
/// clamped. Clients page by asking again from the last parent.
pub const MAX_LOG_LIMIT: u32 = 1000;

/// Serves the control plane from the metadata and object stores.
#[derive(Clone)]
pub(crate) struct ControlServer {
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
}

impl std::fmt::Debug for ControlServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlServer").finish_non_exhaustive()
    }
}

impl ControlServer {
    pub(crate) fn new(meta: Arc<dyn MetadataStore>, objects: Arc<dyn ObjectStore>) -> Self {
        Self { meta, objects }
    }

    fn deriver<'a>(&'a self, repo: &'a str) -> Deriver<'a> {
        Deriver::new(repo, &*self.meta, &*self.objects)
    }

    /// The canonical encodings of `ids`, in order.
    async fn encoded(&self, repo: &str, ids: Vec<ChangesetId>) -> Result<Vec<Vec<u8>>, Status> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let cs = self
                .meta
                .get_changeset(repo, id)
                .await
                .map_err(store_status)?
                .ok_or_else(|| Status::data_loss(format!("indexed changeset {id} is missing")))?;
            out.push(cs.encode_to_vec());
        }
        Ok(out)
    }
}

fn changeset_id(bytes: &[u8], what: &str) -> Result<ChangesetId, Status> {
    ChangesetId::try_from_slice(bytes).map_err(|e| Status::invalid_argument(format!("{what}: {e}")))
}

fn repo_path(s: &str) -> Result<RepoPath, Status> {
    RepoPath::new(s).map_err(|e| Status::invalid_argument(format!("path {s:?}: {e}")))
}

fn limit(n: u32) -> usize {
    n.min(MAX_LOG_LIMIT) as usize
}

/// Map a derivation failure to a gRPC status. A changeset the client named
/// that does not exist is `NOT_FOUND`; anything missing or corrupt below it
/// is the server's data loss.
#[allow(clippy::needless_pass_by_value)] // used as `map_err(derive_status)`
fn derive_status(e: DeriveError) -> Status {
    match e {
        DeriveError::MissingChangeset(id) => Status::not_found(format!("changeset {id}")),
        DeriveError::Store(e) => store_status(e),
        DeriveError::MissingTree(_)
        | DeriveError::MissingObject(_)
        | DeriveError::CorruptObject(_)
        | DeriveError::CorruptPointer { .. } => Status::data_loss(e.to_string()),
    }
}

fn cas_mismatch(expected: u64, actual: u64) -> Status {
    Status::failed_precondition(format!(
        "ref moved: expected version {expected}, found {actual}"
    ))
}

/// Map a failed ref write to a gRPC status.
#[allow(clippy::needless_pass_by_value)] // used as `map_err(ref_status)`
fn ref_status(e: StoreError) -> Status {
    match e {
        StoreError::RefExists(name) => Status::already_exists(format!("ref {name:?}")),
        StoreError::RefNotFound(name) => Status::not_found(format!("ref {name:?}")),
        StoreError::CasMismatch { expected, actual } => cas_mismatch(expected, actual),
        e => store_status(e),
    }
}

fn file_entry(f: scion_core::FileEntry) -> FileEntry {
    FileEntry {
        id: f.id.as_bytes().to_vec(),
        kind: match f.kind {
            scion_types::FileType::Regular => FileType::Regular,
            scion_types::FileType::Executable => FileType::Executable,
            scion_types::FileType::Symlink => FileType::Symlink,
        }
        .into(),
        size: f.size,
    }
}

fn diff_entry(e: scion_core::DiffEntry) -> DiffEntry {
    let (path, old, new) = match e {
        scion_core::DiffEntry::Added { path, new } => (path, None, Some(new)),
        scion_core::DiffEntry::Deleted { path, old } => (path, Some(old), None),
        scion_core::DiffEntry::Modified { path, old, new } => (path, Some(old), Some(new)),
    };
    DiffEntry {
        path: path.as_str().to_owned(),
        old: old.map(file_entry),
        new: new.map(file_entry),
    }
}

/// Store `batch`, in order, and derive what clone, log and diff need, as
/// `CreateChangeset` does for one changeset; returns each id and root tree.
/// Shared with the git importer, which creates changesets on the stores
/// directly. The changesets are stored in one call; their derived objects
/// are then written concurrently and their pointers in one more call, so a
/// batch costs a few round trips however many changesets it holds.
pub async fn create_changesets(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    batch: &[Changeset],
) -> Result<Vec<(ChangesetId, TreeId)>, Status> {
    let Some(repo) = batch.first().map(|cs| cs.repo.as_str()) else {
        return Ok(Vec::new());
    };
    if repo.is_empty() {
        return Err(Status::invalid_argument("changeset names no repository"));
    }
    if let Some(cs) = batch.iter().find(|cs| cs.repo != repo) {
        return Err(Status::invalid_argument(format!(
            "a batch spans repositories {repo:?} and {:?}",
            cs.repo
        )));
    }

    // Every blob must be uploaded first, or blame and checkout would find
    // a hole later.
    let blobs: Vec<BlobId> = batch
        .iter()
        .flat_map(|cs| cs.file_changes.values())
        .filter_map(|change| match change {
            FileChange::Tracked { content, .. } => Some(*content),
            FileChange::Deleted => None,
        })
        .collect();
    let present = objects.has_blobs(&blobs).await.map_err(store_status)?;
    let missing: Vec<String> = blobs
        .iter()
        .zip(present)
        .filter(|(_, present)| !present)
        .map(|(id, _)| id.to_hex())
        .collect();
    if !missing.is_empty() {
        return Err(Status::failed_precondition(format!(
            "blobs not uploaded: {}",
            missing.join(", ")
        )));
    }

    meta.put_changesets(batch).await.map_err(|e| match e {
        StoreError::UnknownParent(_, parent) => {
            Status::failed_precondition(format!("unknown parent {parent}"))
        }
        e => store_status(e),
    })?;

    // Eager derivation: what clone, log and diff need right away. Each
    // costs O(size of the change) given the parents' values. Blame stays
    // lazy; it reads file contents and is only wanted for some paths.
    let staged = Staged::new(repo, meta, objects);
    let ctx = Deriver::new(repo, &staged, &staged);
    let mut created = Vec::with_capacity(batch.len());
    for cs in batch {
        let id = cs.id();
        let root = ctx.derive::<RootTree>(id).await.map_err(derive_status)?;
        ctx.derive::<SkipList>(id).await.map_err(derive_status)?;
        ctx.derive::<PathHistory>(id).await.map_err(derive_status)?;
        created.push((id, root.0));
    }
    staged.store().await.map_err(store_status)?;
    Ok(created)
}

/// Create server ref `name` at `target` (`expected` absent) or move it there
/// fast-forward only if it is still at version `expected`, as `MoveRef`
/// does; returns the new version.
pub async fn move_ref(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    repo: &str,
    name: &str,
    target: ChangesetId,
    expected: Option<u64>,
) -> Result<u64, Status> {
    if !is_allowed_ref_name(name) {
        return Err(Status::invalid_argument(format!(
            "ref name {name:?} is not allowed (only `main` and `release/*`)"
        )));
    }
    let ctx = Deriver::new(repo, meta, objects);

    let Some(expected) = expected else {
        // Creation: the target must be a changeset of this repository.
        ctx.derive::<SkipList>(target)
            .await
            .map_err(derive_status)?;
        let state = meta
            .create_ref(repo, name, target)
            .await
            .map_err(ref_status)?;
        return Ok(state.version);
    };

    let current = meta
        .get_ref(repo, name)
        .await
        .map_err(store_status)?
        .ok_or_else(|| Status::not_found(format!("ref {name:?} in {repo:?}")))?;
    if current.version != expected {
        return Err(cas_mismatch(expected, current.version));
    }
    // The CAS below only succeeds if the ref is still at `expected`, i.e.
    // still at `current.target`, so this check cannot go stale.
    if !SkipList::is_ancestor(&ctx, current.target, target)
        .await
        .map_err(derive_status)?
    {
        return Err(Status::failed_precondition(format!(
            "{target} does not descend from {} ({name}): not a fast-forward",
            current.target
        )));
    }
    let state = meta
        .move_ref(repo, name, expected, target)
        .await
        .map_err(ref_status)?;
    Ok(state.version)
}

#[tonic::async_trait]
impl ControlService for ControlServer {
    async fn get_ref(
        &self,
        request: Request<GetRefRequest>,
    ) -> Result<Response<GetRefResponse>, Status> {
        let req = request.into_inner();
        let state = self
            .meta
            .get_ref(&req.repo, &req.name)
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::not_found(format!("ref {:?} in {:?}", req.name, req.repo)))?;
        let root = self
            .deriver(&req.repo)
            .derive::<RootTree>(state.target)
            .await
            .map_err(derive_status)?;
        Ok(Response::new(GetRefResponse {
            target: state.target.as_bytes().to_vec(),
            version: state.version,
            root_tree: root.0.as_bytes().to_vec(),
        }))
    }

    async fn create_changeset(
        &self,
        request: Request<CreateChangesetRequest>,
    ) -> Result<Response<CreateChangesetResponse>, Status> {
        let cs = Changeset::decode(&request.get_ref().changeset)
            .map_err(|e| Status::invalid_argument(format!("changeset: {e}")))?;
        let [(id, root)] = create_changesets(&*self.meta, &*self.objects, &[cs])
            .await?
            .try_into()
            .expect("one changeset in, one out");
        Ok(Response::new(CreateChangesetResponse {
            id: id.as_bytes().to_vec(),
            root_tree: root.as_bytes().to_vec(),
        }))
    }

    async fn move_ref(
        &self,
        request: Request<MoveRefRequest>,
    ) -> Result<Response<MoveRefResponse>, Status> {
        let req = request.into_inner();
        let target = changeset_id(&req.target, "target")?;
        let version = move_ref(
            &*self.meta,
            &*self.objects,
            &req.repo,
            &req.name,
            target,
            req.expected_version,
        )
        .await?;
        Ok(Response::new(MoveRefResponse { version }))
    }

    async fn diff(&self, request: Request<DiffRequest>) -> Result<Response<DiffResponse>, Status> {
        let req = request.into_inner();
        let old = req
            .old
            .as_deref()
            .map(|b| changeset_id(b, "old"))
            .transpose()?;
        let new = changeset_id(&req.new, "new")?;
        let entries = scion_core::diff(&self.deriver(&req.repo), old, new)
            .await
            .map_err(derive_status)?;
        Ok(Response::new(DiffResponse {
            entries: entries.into_iter().map(diff_entry).collect(),
        }))
    }

    async fn log(&self, request: Request<LogRequest>) -> Result<Response<LogResponse>, Status> {
        let req = request.into_inner();
        let head = changeset_id(&req.head, "head")?;
        let ids = scion_core::log(&self.deriver(&req.repo), head, limit(req.limit))
            .await
            .map_err(derive_status)?;
        Ok(Response::new(LogResponse {
            changesets: self.encoded(&req.repo, ids).await?,
        }))
    }

    async fn path_log(
        &self,
        request: Request<PathLogRequest>,
    ) -> Result<Response<LogResponse>, Status> {
        let req = request.into_inner();
        let head = changeset_id(&req.head, "head")?;
        let path = repo_path(&req.path)?;
        let ids = scion_core::path_log(&self.deriver(&req.repo), head, &path, limit(req.limit))
            .await
            .map_err(derive_status)?;
        Ok(Response::new(LogResponse {
            changesets: self.encoded(&req.repo, ids).await?,
        }))
    }

    async fn merge_base(
        &self,
        request: Request<MergeBaseRequest>,
    ) -> Result<Response<MergeBaseResponse>, Status> {
        let req = request.into_inner();
        let a = changeset_id(&req.a, "a")?;
        let b = changeset_id(&req.b, "b")?;
        let base = SkipList::merge_base(&self.deriver(&req.repo), a, b)
            .await
            .map_err(derive_status)?;
        Ok(Response::new(MergeBaseResponse {
            base: base.map(|id| id.as_bytes().to_vec()),
        }))
    }

    async fn blame(
        &self,
        request: Request<BlameRequest>,
    ) -> Result<Response<BlameResponse>, Status> {
        let req = request.into_inner();
        let cs = changeset_id(&req.changeset, "changeset")?;
        let path = repo_path(&req.path)?;
        let file = Blame::for_path(&self.deriver(&req.repo), cs, &path)
            .await
            .map_err(derive_status)?
            .ok_or_else(|| Status::not_found(format!("no file {path} in {cs}")))?;
        Ok(Response::new(BlameResponse {
            runs: file
                .runs
                .into_iter()
                .map(|(cs, lines)| BlameRun {
                    changeset: cs.as_bytes().to_vec(),
                    lines,
                })
                .collect(),
        }))
    }
}
