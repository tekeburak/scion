//! `ControlService`: thin wrappers over `scion-core` and `scion-derived`.
//! Requests come from clients, so ids, paths and limits are validated here.
//! `CreateChangeset` derives what clone, log and diff need eagerly; anything
//! else is derived on first request.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::{StreamExt, TryStreamExt, stream};

use scion_derived::{Blame, DeriveError, Deriver, PathHistory, RootTree, SkipList};
use scion_proto::v1::control_service_server::ControlService;
use scion_proto::v1::{
    BlameRequest, BlameResponse, BlameRun, ChangeRequest, ChangeRequestStatus,
    CreateChangesetRequest, CreateChangesetResponse, DiffEntry, DiffRequest, DiffResponse,
    FileEntry, FileType, FindLandingRequest, FindLandingResponse, GetChangeRequestRequest,
    GetRefRequest, GetRefResponse, Landing, ListChangeRequestsRequest, ListChangeRequestsResponse,
    LogRequest, LogResponse, MergeBaseRequest, MergeBaseResponse, MoveRefRequest, MoveRefResponse,
    PathLogRequest, RebaseRequest, RebaseResponse, SubmitChangeRequestRequest,
};
use scion_store::{MetadataStore, ObjectStore, StoreError, is_allowed_ref_name};
use scion_types::{BlobId, Changeset, ChangesetId, Decode, Encode, FileChange, RepoPath, TreeId};
use tonic::{Request, Response, Status};

use crate::land::LandWorkers;
use crate::staged::Staged;
use crate::store_status;

/// Most changesets a single `Log` or `PathLog` call returns, and most requests
/// `ListChangeRequests` returns; larger limits are clamped. `FindLanding`
/// refuses more changesets than this. Clients page a log
/// by asking again from the last parent.
pub const MAX_LOG_LIMIT: u32 = 1000;

/// Changesets a `Log` or `PathLog` reply reads at once.
const READ_CONCURRENCY: usize = 32;

/// Serves the control plane from the metadata and object stores.
#[derive(Clone)]
pub(crate) struct ControlServer {
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    workers: Arc<LandWorkers>,
}

impl std::fmt::Debug for ControlServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlServer").finish_non_exhaustive()
    }
}

impl ControlServer {
    pub(crate) fn new(
        meta: Arc<dyn MetadataStore>,
        objects: Arc<dyn ObjectStore>,
        workers: Arc<LandWorkers>,
    ) -> Self {
        Self {
            meta,
            objects,
            workers,
        }
    }

    fn deriver<'a>(&'a self, repo: &'a str) -> Deriver<'a> {
        Deriver::new(repo, &*self.meta, &*self.objects)
    }

    /// The canonical encodings of `ids`, in order.
    /// The changesets `ids`, in order, read concurrently.
    async fn encoded(&self, repo: &str, ids: Vec<ChangesetId>) -> Result<Vec<Vec<u8>>, Status> {
        stream::iter(ids)
            .map(|id| async move {
                let cs = self
                    .meta
                    .get_changeset(repo, id)
                    .await
                    .map_err(store_status)?
                    .ok_or_else(|| {
                        Status::data_loss(format!("indexed changeset {id} is missing"))
                    })?;
                Ok::<_, Status>(cs.encode_to_vec())
            })
            .buffered(READ_CONCURRENCY)
            .try_collect()
            .await
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

fn change_request(cr: scion_store::ChangeRequest) -> ChangeRequest {
    use scion_store::ChangeRequestStatus as S;
    ChangeRequest {
        id: cr.id,
        head: cr.head.as_bytes().to_vec(),
        base: cr.base.as_bytes().to_vec(),
        status: match cr.status {
            S::Pending => ChangeRequestStatus::Pending,
            S::Landing => ChangeRequestStatus::Landing,
            S::Landed => ChangeRequestStatus::Landed,
            S::NeedsRebase => ChangeRequestStatus::NeedsRebase,
            S::Rejected => ChangeRequestStatus::Rejected,
        }
        .into(),
        landed: cr.landed.map(|cs| cs.as_bytes().to_vec()),
        conflicts: cr.conflicts,
        submitted_unix_micros: unix_micros(cr.submitted_at),
        landed_unix_micros: cr.landed_at.map(unix_micros),
    }
}

fn unix_micros(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
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

    // `scn init`: the repository's root is the one changeset that may name a
    // repository that does not exist yet, and storing it creates one.
    if batch[0] == Changeset::repository_root(repo) {
        meta.create_repo(repo).await.map_err(store_status)?;
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

    // Eager derivation: what clone, log and diff need right away. Each
    // costs O(size of the change) given the parents' values. Blame stays
    // lazy; it reads file contents and is only wanted for some paths. The
    // batch's rows are inserted, the objects of its trees and path history
    // written, and its skip lists (pointers only, one metadata read per
    // level) derived all at once; the pointers are recorded once all three
    // are done.
    let staged = Staged::new(repo, meta, objects, batch);
    let ctx = Deriver::new(repo, &staged, &staged);
    let trees = async {
        let mut created = Vec::with_capacity(batch.len());
        for cs in batch {
            let id = cs.id();
            let root = ctx.derive::<RootTree>(id).await.map_err(derive_status)?;
            ctx.derive::<PathHistory>(id).await.map_err(derive_status)?;
            created.push((id, root.0));
        }
        staged.store_objects().await.map_err(store_status)?;
        Ok::<_, Status>(created)
    };
    let skip_lists = async {
        for cs in batch {
            ctx.derive::<SkipList>(cs.id()).await?;
        }
        Ok(())
    };
    let (inserted, created, skipped) = tokio::join!(meta.put_changesets(batch), trees, skip_lists);
    // An unknown parent also fails the derivation; the insert names it.
    inserted.map_err(|e| match e {
        StoreError::UnknownParent(_, parent) => {
            Status::failed_precondition(format!("unknown parent {parent}"))
        }
        e => store_status(e),
    })?;
    let created = created?;
    skipped.map_err(derive_status)?;
    staged.store_pointers().await.map_err(store_status)?;
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

/// What [`rebase`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebased {
    /// Every changeset was replayed: `head` is the new head, with root tree
    /// `root`, and `created` changesets were created for it.
    Clean {
        head: ChangesetId,
        root: TreeId,
        created: usize,
    },
    /// Replaying `changeset` conflicts on `paths` (sorted); nothing after it
    /// was replayed.
    Conflict {
        changeset: ChangesetId,
        paths: Vec<RepoPath>,
    },
}

/// Replay `head`'s changesets that `onto` does not contain on top of
/// `onto`, oldest first, through [`scion_core::rebase`], creating each as
/// `CreateChangeset` does; as the `Rebase` RPC does.
pub async fn rebase(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    repo: &str,
    head: ChangesetId,
    onto: ChangesetId,
) -> Result<Rebased, Status> {
    let ctx = Deriver::new(repo, meta, objects);
    let clean = async |head: ChangesetId, created| {
        let root = ctx.derive::<RootTree>(head).await.map_err(derive_status)?.0;
        Ok(Rebased::Clean {
            head,
            root,
            created,
        })
    };
    if SkipList::is_ancestor(&ctx, onto, head)
        .await
        .map_err(derive_status)?
    {
        return clean(head, 0).await;
    }

    // The stack to replay: `head`'s first parents down to the first changeset
    // `onto` contains, bounded so a stray `onto` cannot walk the log.
    let mut stack = Vec::new();
    let mut cur = head;
    while !SkipList::is_ancestor(&ctx, cur, onto)
        .await
        .map_err(derive_status)?
    {
        if stack.len() == limit(MAX_LOG_LIMIT) {
            return Err(Status::failed_precondition(format!(
                "more than {MAX_LOG_LIMIT} changesets of {head} are not in {onto}"
            )));
        }
        let cs = meta
            .get_changeset(repo, cur)
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::data_loss(format!("indexed changeset {cur} is missing")))?;
        let parent = match cs.parents[..] {
            [parent] => parent,
            [] => {
                return Err(Status::failed_precondition(format!(
                    "{head} shares no history with {onto}"
                )));
            }
            _ => {
                return Err(Status::failed_precondition(format!(
                    "{cur} is a merge; only a linear stack can be replayed"
                )));
            }
        };
        stack.push(cs);
        cur = parent;
    }

    let mut parent = onto;
    let mut created = 0;
    for cs in stack.into_iter().rev() {
        let id = cs.id();
        let merged = scion_core::rebase(&ctx, id, parent)
            .await
            .map_err(derive_status)?;
        if !merged.is_clean() {
            let mut paths: Vec<RepoPath> =
                merged.conflicts.iter().map(|c| c.path().clone()).collect();
            paths.sort();
            paths.dedup();
            return Ok(Rebased::Conflict {
                changeset: id,
                paths,
            });
        }
        // Everything it changed is there already.
        if merged.changes.is_empty() {
            continue;
        }
        let replayed = Changeset {
            parents: vec![parent],
            file_changes: merged.changes,
            ..cs
        };
        let [(new, _)] = create_changesets(meta, objects, &[replayed])
            .await?
            .try_into()
            .expect("one changeset in, one out");
        parent = new;
        created += 1;
    }
    clean(parent, created).await
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

    async fn rebase(
        &self,
        request: Request<RebaseRequest>,
    ) -> Result<Response<RebaseResponse>, Status> {
        let req = request.into_inner();
        let head = changeset_id(&req.head, "head")?;
        let onto = changeset_id(&req.onto, "onto")?;
        let resp = match rebase(&*self.meta, &*self.objects, &req.repo, head, onto).await? {
            Rebased::Clean {
                head,
                root,
                created,
            } => RebaseResponse {
                head: Some(head.as_bytes().to_vec()),
                root_tree: root.as_bytes().to_vec(),
                created: u32::try_from(created).expect("bounded by MAX_LOG_LIMIT"),
                ..RebaseResponse::default()
            },
            Rebased::Conflict { changeset, paths } => RebaseResponse {
                conflicted: changeset.as_bytes().to_vec(),
                conflicts: paths.iter().map(|p| p.as_str().to_owned()).collect(),
                ..RebaseResponse::default()
            },
        };
        Ok(Response::new(resp))
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

    async fn submit_change_request(
        &self,
        request: Request<SubmitChangeRequestRequest>,
    ) -> Result<Response<ChangeRequest>, Status> {
        let req = request.into_inner();
        let head = changeset_id(&req.head, "head")?;
        if self
            .meta
            .get_changeset(&req.repo, head)
            .await
            .map_err(store_status)?
            .is_none()
        {
            return Err(Status::not_found(format!(
                "changeset {head} in {:?}",
                req.repo
            )));
        }
        // Every repository gets `main` from `scn init`; without it the
        // repository does not exist.
        let main = self
            .meta
            .get_ref(&req.repo, "main")
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::not_found(format!("ref \"main\" in {:?}", req.repo)))?;
        let mut cr = self
            .meta
            .submit_change_request(&req.repo, head, main.target)
            .await
            .map_err(store_status)?;
        self.workers.wake(&req.repo);
        if req.wait {
            cr = self
                .workers
                .wait(&req.repo, cr.id)
                .await
                .map_err(store_status)?
                .ok_or_else(|| Status::internal(format!("change request {} vanished", cr.id)))?;
        }
        Ok(Response::new(change_request(cr)))
    }

    async fn get_change_request(
        &self,
        request: Request<GetChangeRequestRequest>,
    ) -> Result<Response<ChangeRequest>, Status> {
        let req = request.into_inner();
        let cr = if req.wait {
            self.workers.wait(&req.repo, req.id).await
        } else {
            self.meta.get_change_request(&req.repo, req.id).await
        }
        .map_err(store_status)?
        .ok_or_else(|| Status::not_found(format!("change request {} in {:?}", req.id, req.repo)))?;
        Ok(Response::new(change_request(cr)))
    }

    async fn list_change_requests(
        &self,
        request: Request<ListChangeRequestsRequest>,
    ) -> Result<Response<ListChangeRequestsResponse>, Status> {
        use scion_store::ChangeRequestStatus as S;
        let req = request.into_inner();
        let status = match ChangeRequestStatus::try_from(req.status) {
            Ok(ChangeRequestStatus::Unspecified) => None,
            Ok(ChangeRequestStatus::Pending) => Some(S::Pending),
            Ok(ChangeRequestStatus::Landing) => Some(S::Landing),
            Ok(ChangeRequestStatus::Landed) => Some(S::Landed),
            Ok(ChangeRequestStatus::NeedsRebase) => Some(S::NeedsRebase),
            Ok(ChangeRequestStatus::Rejected) => Some(S::Rejected),
            Err(_) => {
                return Err(Status::invalid_argument(format!(
                    "change request status {}",
                    req.status
                )));
            }
        };
        let requests = self
            .meta
            .list_change_requests(&req.repo, status, req.limit.min(MAX_LOG_LIMIT))
            .await
            .map_err(store_status)?;
        Ok(Response::new(ListChangeRequestsResponse {
            requests: requests.into_iter().map(change_request).collect(),
        }))
    }

    async fn find_landing(
        &self,
        request: Request<FindLandingRequest>,
    ) -> Result<Response<FindLandingResponse>, Status> {
        let req = request.into_inner();
        if req.changesets.len() > MAX_LOG_LIMIT as usize {
            return Err(Status::invalid_argument(format!(
                "{} changesets; at most {MAX_LOG_LIMIT}",
                req.changesets.len()
            )));
        }
        let changesets = req
            .changesets
            .iter()
            .map(|cs| changeset_id(cs, "changeset"))
            .collect::<Result<Vec<_>, _>>()?;
        let landings = self
            .meta
            .find_landings(&req.repo, &changesets)
            .await
            .map_err(store_status)?;
        Ok(Response::new(FindLandingResponse {
            landings: landings
                .into_iter()
                .map(|(cs, cr)| Landing {
                    changeset: cs.as_bytes().to_vec(),
                    request: Some(change_request(cr)),
                })
                .collect(),
        }))
    }
}
