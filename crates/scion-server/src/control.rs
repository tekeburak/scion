//! `ControlService`: thin wrappers over `scion-core` and `scion-derived`.
//! Requests come from clients, so ids, paths and limits are validated here.
//! Derived data is computed on first request if it is not stored yet.

use std::sync::Arc;

use scion_derived::{Blame, DeriveError, Deriver, RootTree, SkipList};
use scion_proto::v1::control_service_server::ControlService;
use scion_proto::v1::{
    BlameRequest, BlameResponse, BlameRun, CreateChangesetRequest, CreateChangesetResponse,
    DiffEntry, DiffRequest, DiffResponse, FileEntry, FileType, GetRefRequest, GetRefResponse,
    LogRequest, LogResponse, MergeBaseRequest, MergeBaseResponse, MoveRefRequest, MoveRefResponse,
    PathLogRequest,
};
use scion_store::{MetadataStore, ObjectStore};
use scion_types::{ChangesetId, Encode, RepoPath};
use tonic::{Request, Response, Status};

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
        _request: Request<CreateChangesetRequest>,
    ) -> Result<Response<CreateChangesetResponse>, Status> {
        // TODO(Phase 3.4): blob check, WAL write, eager derivation.
        Err(Status::unimplemented(
            "CreateChangeset arrives in Phase 3.4",
        ))
    }

    async fn move_ref(
        &self,
        _request: Request<MoveRefRequest>,
    ) -> Result<Response<MoveRefResponse>, Status> {
        // TODO(Phase 3.5): fast-forward-only compare-and-swap.
        Err(Status::unimplemented("MoveRef arrives in Phase 3.5"))
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
