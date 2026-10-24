//! `ControlService`: thin wrappers over `scion-core` and `scion-derived`.
//! Requests come from clients, so ids, paths and limits are validated here.
//! `CreateChangeset` derives what clone, log and diff need eagerly
//! (`CommitAndLand` the path history just after its reply); anything else is
//! derived on first request. Each RPC first checks the caller's role
//! in the repository it names.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::{StreamExt, TryStreamExt, stream};

use scion_derived::{Blame, DeriveError, Deriver, FileBlame, PathHistory, RootTree, SkipList};
use scion_proto::trace::{span, timed};
use scion_proto::v1::control_service_server::ControlService;
use scion_proto::v1::{
    BlameRequest, BlameResponse, BlameRun, ChangeRequest, ChangeRequestStatus,
    CommitAndLandRequest, CreateChangesetRequest, CreateChangesetResponse, DiffEntry, DiffRequest,
    DiffResponse, FileEntry, FileType, FindLandingRequest, FindLandingResponse,
    GetChangeRequestRequest, GetRefRequest, GetRefResponse, GrantRequest, GrantResponse,
    InitRepoRequest, InitRepoResponse, Landing, ListChangeRequestsRequest,
    ListChangeRequestsResponse, ListGrantsRequest, ListGrantsResponse, LogRequest, LogResponse,
    MergeBaseRequest, MergeBaseResponse, MergeRequest, MergeResponse, MoveRefRequest,
    MoveRefResponse, PathLogRequest, RebaseRequest, RebaseResponse, RevertRequest, RevertResponse,
    RevokeRequest, RevokeResponse, SubmitChangeRequestRequest, WhoAmIRequest, WhoAmIResponse,
};
use scion_store::{
    Grant, MetadataStore, ObjectStore, Role, SERVER_GRANTS, StoreError, is_allowed_ref_name,
};
use scion_types::{
    BlobId, Changeset, ChangesetId, Decode, FileChange, Identity, RepoPath, Timestamp, TreeEntry,
    TreeId,
};
use tonic::{Request, Response, Status};

use crate::auth::{Scope, authorize, principal};
use crate::land::LandWorkers;
use crate::staged::Staged;
use crate::store_status;

/// Most changesets a single `Log` or `PathLog` call returns, and most requests
/// `ListChangeRequests` returns; larger limits are clamped. `FindLanding`
/// refuses more changesets than this. Clients page a log
/// by asking again from the last parent.
pub const MAX_LOG_LIMIT: u32 = 1000;

/// Tree lookups a `Revert` runs at once.
const READ_CONCURRENCY: usize = 32;

/// Blame runs remembered while their nodes are derived; about 8 MiB.
const BLAME_MEMO_RUNS: u64 = 200_000;

/// A file's attribution as the changeset that last changed it left it, keyed
/// by repository, that changeset and the path.
type BlameMemo = moka::sync::Cache<(String, ChangesetId, RepoPath), FileBlame>;

/// Bytes of path log replies remembered.
const PATH_LOG_MEMO_BYTES: u64 = 64 << 20;

/// The changesets a `PathLog` answered, encoded, keyed by repository, head,
/// path and limit.
type PathLogMemo = moka::sync::Cache<(String, ChangesetId, RepoPath, usize), Vec<Vec<u8>>>;

/// Serves the control plane from the metadata and object stores.
#[derive(Clone)]
pub(crate) struct ControlServer {
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    workers: Arc<LandWorkers>,
    /// Attributions `Blame` answered before the node holding them was
    /// stored, so a repeated request does not attribute the file again
    /// while that node is derived. They are immutable, like the node.
    blames: BlameMemo,
    /// Replies `PathLog` gave. A head's history never changes, so neither
    /// does the reply.
    path_logs: PathLogMemo,
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
            blames: moka::sync::Cache::builder()
                .max_capacity(BLAME_MEMO_RUNS)
                .weigher(|_, file: &FileBlame| u32::try_from(file.runs.len()).unwrap_or(u32::MAX))
                .build(),
            path_logs: moka::sync::Cache::builder()
                .max_capacity(PATH_LOG_MEMO_BYTES)
                .weigher(|_, reply: &Vec<Vec<u8>>| {
                    u32::try_from(reply.iter().map(Vec::len).sum::<usize>()).unwrap_or(u32::MAX)
                })
                .build(),
        }
    }

    fn deriver<'a>(&'a self, repo: &'a str) -> Deriver<'a> {
        Deriver::new(repo, &*self.meta, &*self.objects)
    }

    /// The canonical encodings of `ids`, in order, read with one
    /// `get_changesets`.
    async fn encoded(&self, repo: &str, ids: Vec<ChangesetId>) -> Result<Vec<Vec<u8>>, Status> {
        let found = self
            .meta
            .get_encoded_changesets(repo, &ids)
            .await
            .map_err(store_status)?;
        ids.iter()
            .zip(found)
            .map(|(id, cs)| {
                cs.ok_or_else(|| Status::data_loss(format!("indexed changeset {id} is missing")))
            })
            .collect()
    }
}

impl ControlServer {
    /// The attribution of `path` at `cs`. When the node of the changeset
    /// that last changed it is not stored, the reply waits on this file's
    /// versions only: the node, which holds every file that changeset
    /// changed, is derived and stored by a task, gently, since nothing
    /// waits on it.
    async fn attribute(
        &self,
        repo: &str,
        cs: ChangesetId,
        path: &RepoPath,
    ) -> Result<FileBlame, Status> {
        let deriver = self.deriver(repo);
        let last_changed = Blame::last_changed(&deriver, cs, path)
            .await
            .map_err(derive_status)?
            .ok_or_else(|| Status::not_found(format!("no file {path} in {cs}")))?;
        let key = (repo.to_owned(), last_changed, path.clone());
        if let Some(file) = self.blames.get(&key) {
            return Ok(file);
        }
        if let Some(file) = Blame::stored_file(&deriver, last_changed, path)
            .await
            .map_err(derive_status)?
        {
            return Ok(file);
        }
        let file = Blame::file_alone(&deriver, last_changed, path)
            .await
            .map_err(derive_status)?;
        self.blames.insert(key, file.clone());
        let (meta, objects, repo) = (self.meta.clone(), self.objects.clone(), repo.to_owned());
        tokio::spawn(async move {
            let deriver = Deriver::new(&repo, &*meta, &*objects);
            if let Err(e) = timed("blame", Blame::store_gently(&deriver, last_changed)).await {
                eprintln!("scion-server: blame of {last_changed} in {repo:?}: {e}");
            }
        });
        Ok(file)
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
    create(meta, objects, batch, true).await
}

/// [`create_changesets`], deriving path history too only if `history`:
/// landing waits on the root tree and the skip list, not on it.
async fn create(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    batch: &[Changeset],
    history: bool,
) -> Result<Vec<(ChangesetId, TreeId)>, Status> {
    let _create = span("create");
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
    let present = timed("has_blobs", objects.has_blobs(&blobs))
        .await
        .map_err(store_status)?;
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
        let derive = span("derive");
        let mut created = Vec::with_capacity(batch.len());
        for cs in batch {
            let id = cs.id();
            let root = ctx.derive::<RootTree>(id).await.map_err(derive_status)?;
            if history {
                ctx.derive::<PathHistory>(id).await.map_err(derive_status)?;
            }
            created.push((id, root.0));
        }
        drop(derive);
        timed("store", staged.store_objects())
            .await
            .map_err(store_status)?;
        Ok::<_, Status>(created)
    };
    let skip_lists = timed("skip_list", async {
        for cs in batch {
            ctx.derive::<SkipList>(cs.id()).await?;
        }
        Ok(())
    });
    let (inserted, created, skipped) = tokio::join!(
        timed("insert", meta.put_changesets(batch)),
        trees,
        skip_lists
    );
    // An unknown parent also fails the derivation; the insert names it.
    inserted.map_err(|e| match e {
        StoreError::UnknownParent(_, parent) => {
            Status::failed_precondition(format!("unknown parent {parent}"))
        }
        e => store_status(e),
    })?;
    let created = created?;
    skipped.map_err(derive_status)?;
    timed("pointers", staged.store_pointers())
        .await
        .map_err(store_status)?;
    Ok(created)
}

/// Derive and store, on a task, the path history of changeset `id` of
/// `repo`, which is stored; until the task has, `PathLog` and `Blame`
/// derive it on first use.
pub(crate) fn spawn_history(
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    repo: String,
    id: ChangesetId,
) {
    tokio::spawn(async move {
        let stored = async {
            let cs = meta
                .get_changeset(&repo, id)
                .await
                .map_err(store_status)?
                .ok_or_else(|| Status::data_loss(format!("changeset {id} is missing")))?;
            store_history(&*meta, &*objects, &cs).await
        };
        if let Err(e) = timed("history", stored).await {
            eprintln!("scion-server: path history of {id} in {repo:?}: {e}");
        }
    });
}

/// Derive the path history of `cs`, which is stored, and store it in one
/// write of its pages and one of its pointer, as [`create_changesets`] does.
async fn store_history(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    cs: &Changeset,
) -> Result<(), Status> {
    let batch = std::slice::from_ref(cs);
    let staged = Staged::new(&cs.repo, meta, objects, batch);
    Deriver::new(&cs.repo, &staged, &staged)
        .derive::<PathHistory>(cs.id())
        .await
        .map_err(derive_status)?;
    staged.store_objects().await.map_err(store_status)?;
    staged.store_pointers().await.map_err(store_status)
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
    let _move_ref = span("move_ref");
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
    /// The stack changes these paths (sorted), outside the scope; nothing
    /// was replayed.
    Denied { paths: Vec<RepoPath> },
}

/// Replay `head`'s changesets that `onto` does not contain on top of
/// `onto`, oldest first, through [`scion_core::rebase`], creating each as
/// `CreateChangeset` does; as the `Rebase` RPC does. The stack is `head`'s
/// first-parent line; a merge in it is replayed against its first parent
/// and keeps its other parents. Nothing is created if the stack changes a
/// path outside `scope`, or when `onto` is on `head`'s first-parent line
/// already. A `head` that reaches `onto` only through a merge's other
/// parent is replayed, so `onto` stays on the result's first-parent line:
/// landing it as it is would take `main`'s history off `main`'s
/// first-parent line.
pub async fn rebase(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    repo: &str,
    head: ChangesetId,
    onto: ChangesetId,
    scope: &Scope,
) -> Result<Rebased, Status> {
    replay(meta, objects, repo, (head, onto), scope, true).await
}

/// [`rebase`], deriving the path history of the changesets it creates only
/// if `history`: the land worker moves `main` on the root tree and the skip
/// list, and stores the history afterwards.
pub(crate) async fn replay(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    repo: &str,
    (head, onto): (ChangesetId, ChangesetId),
    scope: &Scope,
    history: bool,
) -> Result<Rebased, Status> {
    let _rebase = span("rebase");
    let ctx = Deriver::new(repo, meta, objects);
    let clean = async |head: ChangesetId, created| {
        let root = ctx.derive::<RootTree>(head).await.map_err(derive_status)?.0;
        Ok(Rebased::Clean {
            head,
            root,
            created,
        })
    };
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
        let Some(parent) = cs.first_parent() else {
            return Err(Status::failed_precondition(format!(
                "{head} shares no history with {onto}"
            )));
        };
        stack.push(cs);
        cur = parent;
    }
    let paths = scope.denied(stack.iter().flat_map(|cs| cs.file_changes.keys()));
    if !paths.is_empty() {
        return Ok(Rebased::Denied { paths });
    }
    if cur == onto {
        return clean(head, 0).await;
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
        // A merge keeps its other parents; its changes are against the
        // first one only, as they were.
        let mut parents = cs.parents.clone();
        parents[0] = parent;
        let replayed = Changeset {
            parents,
            file_changes: merged.changes,
            ..cs
        };
        let [(new, _)] = create(meta, objects, &[replayed], history)
            .await?
            .try_into()
            .expect("one changeset in, one out");
        parent = new;
        created += 1;
    }
    clean(parent, created).await
}

/// What [`merge`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    /// `merged`, with root tree `root`, has both heads; `created` unless
    /// `ours` already contained `theirs`.
    Clean {
        merged: ChangesetId,
        root: TreeId,
        created: bool,
    },
    /// The heads conflict on `paths` (sorted); nothing was created.
    Conflict { paths: Vec<RepoPath> },
}

/// Merge `theirs` into `ours` through [`scion_core::merge`] and create the
/// merge changeset, parents `[ours, theirs]` and file changes against `ours`
/// only, as `CreateChangeset` does; as the `Merge` RPC does. A merge that
/// changes a path outside `scope` is refused with `PERMISSION_DENIED`.
pub async fn merge(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    repo: &str,
    (ours, theirs): (ChangesetId, ChangesetId),
    (author, author_date): (Identity, Timestamp),
    message: &str,
    scope: &Scope,
) -> Result<Merged, Status> {
    let ctx = Deriver::new(repo, meta, objects);
    if SkipList::is_ancestor(&ctx, theirs, ours)
        .await
        .map_err(derive_status)?
    {
        let root = ctx.derive::<RootTree>(ours).await.map_err(derive_status)?.0;
        return Ok(Merged::Clean {
            merged: ours,
            root,
            created: false,
        });
    }
    let merged = scion_core::merge(&ctx, ours, theirs)
        .await
        .map_err(derive_status)?;
    if !merged.is_clean() {
        let mut paths: Vec<RepoPath> = merged.conflicts.iter().map(|c| c.path().clone()).collect();
        paths.sort();
        paths.dedup();
        return Ok(Merged::Conflict { paths });
    }
    scope.check("change", merged.changes.keys())?;
    let cs = Changeset {
        repo: repo.to_owned(),
        parents: vec![ours, theirs],
        author,
        author_date,
        committer: None,
        committer_date: None,
        message: message.to_owned(),
        file_changes: merged.changes,
        extra: std::collections::BTreeMap::new(),
    };
    let [(merged, root)] = create_changesets(meta, objects, &[cs])
        .await?
        .try_into()
        .expect("one changeset in, one out");
    Ok(Merged::Clean {
        merged,
        root,
        created: true,
    })
}

/// What [`revert`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reverted {
    /// `reverted`, with root tree `root`, undoes the changeset on the head.
    Clean { reverted: ChangesetId, root: TreeId },
    /// These paths (sorted) changed between the changeset and the head;
    /// nothing was created.
    ChangedSince { paths: Vec<RepoPath> },
}

/// Undo `target`'s file changes on `head`, a descendant of it: create a
/// changeset on `head` that puts every path `target` changed back as it was
/// at `target`'s first parent, as `CreateChangeset` does; as the `Revert`
/// RPC does. Costs three path lookups per file `target` changed, whatever
/// lies between it and `head`. A revert that changes a path outside `scope`
/// is refused with `PERMISSION_DENIED`.
pub async fn revert(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    repo: &str,
    (head, target): (ChangesetId, ChangesetId),
    (author, author_date): (Identity, Timestamp),
    message: &str,
    scope: &Scope,
) -> Result<Reverted, Status> {
    let ctx = Deriver::new(repo, meta, objects);
    if !SkipList::is_ancestor(&ctx, target, head)
        .await
        .map_err(derive_status)?
    {
        return Err(Status::failed_precondition(format!(
            "{target} is not in the history of {head}"
        )));
    }
    let cs = meta
        .get_changeset(repo, target)
        .await
        .map_err(store_status)?
        .ok_or_else(|| Status::data_loss(format!("indexed changeset {target} is missing")))?;
    let parent = cs.first_parent();
    let at_head = ctx.derive::<RootTree>(head).await.map_err(derive_status)?;
    let at_target = ctx
        .derive::<RootTree>(target)
        .await
        .map_err(derive_status)?;
    let before = match parent {
        Some(p) => Some(ctx.derive::<RootTree>(p).await.map_err(derive_status)?),
        None => None,
    };
    let entries: Vec<_> = stream::iter(cs.file_changes.into_keys())
        .map(|path| async move {
            let (now, then) = futures_util::try_join!(
                at_head.lookup(&ctx, &path),
                at_target.lookup(&ctx, &path)
            )?;
            let old = match before {
                Some(tree) => tree.lookup(&ctx, &path).await?,
                None => None,
            };
            Ok::<_, DeriveError>((path, now, then, old))
        })
        .buffered(READ_CONCURRENCY)
        .try_collect()
        .await
        .map_err(derive_status)?;

    let mut changed_since = Vec::new();
    let mut file_changes = std::collections::BTreeMap::new();
    for (path, now, then, old) in entries {
        if now != then {
            changed_since.push(path);
            continue;
        }
        // `then == old` only for a change that changed nothing.
        if then == old {
            continue;
        }
        let restored =
            if let (Some(TreeEntry::Blob { id, kind, size }), Some(parent)) = (old, parent) {
                FileChange::Tracked {
                    content: id,
                    kind,
                    size,
                    copy_from: Some((path.clone(), parent)),
                }
            } else if matches!(then, Some(TreeEntry::Blob { .. })) {
                // Not a file before `target` (a directory or nothing).
                FileChange::Deleted
            } else {
                continue;
            };
        file_changes.insert(path, restored);
    }
    if !changed_since.is_empty() {
        return Ok(Reverted::ChangedSince {
            paths: changed_since,
        });
    }
    if file_changes.is_empty() {
        return Err(Status::failed_precondition(format!(
            "{target} changes no file"
        )));
    }
    scope.check("change", file_changes.keys())?;
    let cs = Changeset {
        repo: repo.to_owned(),
        parents: vec![head],
        author,
        author_date,
        committer: None,
        committer_date: None,
        message: message.to_owned(),
        file_changes,
        extra: std::collections::BTreeMap::new(),
    };
    let [(reverted, root)] = create_changesets(meta, objects, &[cs])
        .await?
        .try_into()
        .expect("one changeset in, one out");
    Ok(Reverted::Clean { reverted, root })
}

#[tonic::async_trait]
impl ControlService for ControlServer {
    async fn init_repo(
        &self,
        request: Request<InitRepoRequest>,
    ) -> Result<Response<InitRepoResponse>, Status> {
        let _rpc = span("InitRepo");
        timed(
            "auth",
            authorize(&*self.meta, &request, SERVER_GRANTS, Role::Admin),
        )
        .await?;
        let admin = principal(&request)?.to_owned();
        let repo = request.into_inner().repo;
        let [(root, _)] = create_changesets(
            &*self.meta,
            &*self.objects,
            &[Changeset::repository_root(&repo)],
        )
        .await?
        .try_into()
        .expect("one changeset in, one out");
        let version = move_ref(&*self.meta, &*self.objects, &repo, "main", root, None)
            .await
            .map_err(|st| match st.code() {
                tonic::Code::AlreadyExists => {
                    Status::already_exists(format!("repository {repo:?} already exists"))
                }
                _ => st,
            })?;
        // After `main`, so a name that was taken never gains an admin; a
        // crash in between leaves a repository for a server admin to grant.
        let grant = Grant {
            principal: admin,
            path_prefix: String::new(),
            role: Role::Admin,
        };
        self.meta
            .put_grant(&repo, &grant)
            .await
            .map_err(store_status)?;
        Ok(Response::new(InitRepoResponse {
            main: root.as_bytes().to_vec(),
            version,
        }))
    }

    async fn get_ref(
        &self,
        request: Request<GetRefRequest>,
    ) -> Result<Response<GetRefResponse>, Status> {
        let _rpc = span("GetRef");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
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
        let _rpc = span("CreateChangeset");
        let cs = Changeset::decode(&request.get_ref().changeset)
            .map_err(|e| Status::invalid_argument(format!("changeset: {e}")))?;
        timed(
            "auth",
            authorize(&*self.meta, &request, &cs.repo, Role::Writer),
        )
        .await?
        .check("change", cs.file_changes.keys())?;
        if cs == Changeset::repository_root(&cs.repo) {
            return Err(Status::invalid_argument(
                "a repository's root changeset is created by InitRepo (`scn init`)",
            ));
        }
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
        let _rpc = span("MoveRef");
        let scope = timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Admin),
        )
        .await?;
        if !scope.is_whole() {
            return Err(Status::permission_denied(format!(
                "{} is an admin of directories of {:?} only; moving a ref needs the whole repository",
                principal(&request)?,
                request.get_ref().repo
            )));
        }
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
        let _rpc = span("Diff");
        let scope = timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
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
        // Only the paths the caller may read.
        Ok(Response::new(DiffResponse {
            entries: entries
                .into_iter()
                .filter(|e| scope.allows(e.path()))
                .map(diff_entry)
                .collect(),
        }))
    }

    async fn log(&self, request: Request<LogRequest>) -> Result<Response<LogResponse>, Status> {
        let _rpc = span("Log");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
        let req = request.into_inner();
        let head = changeset_id(&req.head, "head")?;
        let ids = timed(
            "walk",
            scion_core::log(&self.deriver(&req.repo), head, limit(req.limit)),
        )
        .await
        .map_err(derive_status)?;
        Ok(Response::new(LogResponse {
            changesets: timed("read", self.encoded(&req.repo, ids)).await?,
        }))
    }

    async fn path_log(
        &self,
        request: Request<PathLogRequest>,
    ) -> Result<Response<LogResponse>, Status> {
        let _rpc = span("PathLog");
        let scope = timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
        let req = request.into_inner();
        let head = changeset_id(&req.head, "head")?;
        let path = repo_path(&req.path)?;
        scope.check("read", [&path])?;
        let key = (req.repo, head, path, limit(req.limit));
        if let Some(changesets) = self.path_logs.get(&key) {
            return Ok(Response::new(LogResponse { changesets }));
        }
        let (repo, head, path, limit) = &key;
        let ids = timed(
            "walk",
            scion_core::path_log(&self.deriver(repo), *head, path, *limit),
        )
        .await
        .map_err(derive_status)?;
        let changesets = timed("read", self.encoded(repo, ids)).await?;
        self.path_logs.insert(key, changesets.clone());
        Ok(Response::new(LogResponse { changesets }))
    }

    async fn merge_base(
        &self,
        request: Request<MergeBaseRequest>,
    ) -> Result<Response<MergeBaseResponse>, Status> {
        let _rpc = span("MergeBase");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
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
        let _rpc = span("Rebase");
        let scope = timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Writer),
        )
        .await?;
        let req = request.into_inner();
        let head = changeset_id(&req.head, "head")?;
        let onto = changeset_id(&req.onto, "onto")?;
        let resp = match rebase(&*self.meta, &*self.objects, &req.repo, head, onto, &scope).await? {
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
            Rebased::Denied { paths } => return Err(scope.refuse("change", &paths[0])),
        };
        Ok(Response::new(resp))
    }

    async fn merge(
        &self,
        request: Request<MergeRequest>,
    ) -> Result<Response<MergeResponse>, Status> {
        let _rpc = span("Merge");
        let scope = timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Writer),
        )
        .await?;
        let req = request.into_inner();
        let ours = changeset_id(&req.ours, "ours")?;
        let theirs = changeset_id(&req.theirs, "theirs")?;
        let author = (
            Identity::new(req.author_name, req.author_email),
            Timestamp::new(req.author_unix_secs, req.author_tz_offset_minutes),
        );
        let resp = match merge(
            &*self.meta,
            &*self.objects,
            &req.repo,
            (ours, theirs),
            author,
            &req.message,
            &scope,
        )
        .await?
        {
            Merged::Clean {
                merged,
                root,
                created,
            } => MergeResponse {
                merged: Some(merged.as_bytes().to_vec()),
                root_tree: root.as_bytes().to_vec(),
                created,
                ..MergeResponse::default()
            },
            Merged::Conflict { paths } => MergeResponse {
                conflicts: paths.iter().map(|p| p.as_str().to_owned()).collect(),
                ..MergeResponse::default()
            },
        };
        Ok(Response::new(resp))
    }

    async fn revert(
        &self,
        request: Request<RevertRequest>,
    ) -> Result<Response<RevertResponse>, Status> {
        let _rpc = span("Revert");
        let scope = timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Writer),
        )
        .await?;
        let req = request.into_inner();
        let head = changeset_id(&req.head, "head")?;
        let target = changeset_id(&req.changeset, "changeset")?;
        let author = (
            Identity::new(req.author_name, req.author_email),
            Timestamp::new(req.author_unix_secs, req.author_tz_offset_minutes),
        );
        let resp = match revert(
            &*self.meta,
            &*self.objects,
            &req.repo,
            (head, target),
            author,
            &req.message,
            &scope,
        )
        .await?
        {
            Reverted::Clean { reverted, root } => RevertResponse {
                reverted: Some(reverted.as_bytes().to_vec()),
                root_tree: root.as_bytes().to_vec(),
                ..RevertResponse::default()
            },
            Reverted::ChangedSince { paths } => RevertResponse {
                changed_since: paths.iter().map(|p| p.as_str().to_owned()).collect(),
                ..RevertResponse::default()
            },
        };
        Ok(Response::new(resp))
    }

    async fn blame(
        &self,
        request: Request<BlameRequest>,
    ) -> Result<Response<BlameResponse>, Status> {
        let _rpc = span("Blame");
        let scope = timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
        let req = request.into_inner();
        let cs = changeset_id(&req.changeset, "changeset")?;
        let path = repo_path(&req.path)?;
        scope.check("read", [&path])?;
        let file = timed("attribute", self.attribute(&req.repo, cs, &path)).await?;
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
        let _rpc = span("SubmitChangeRequest");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Writer),
        )
        .await?;
        let submitter = principal(&request)?.to_owned();
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
            .submit_change_request(&req.repo, head, main.target, &submitter)
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

    async fn commit_and_land(
        &self,
        request: Request<CommitAndLandRequest>,
    ) -> Result<Response<ChangeRequest>, Status> {
        let _rpc = span("CommitAndLand");
        let cs = Changeset::decode(&request.get_ref().changeset)
            .map_err(|e| Status::invalid_argument(format!("changeset: {e}")))?;
        timed(
            "auth",
            authorize(&*self.meta, &request, &cs.repo, Role::Writer),
        )
        .await?
        .check("change", cs.file_changes.keys())?;
        if cs == Changeset::repository_root(&cs.repo) {
            return Err(Status::invalid_argument(
                "a repository's root changeset is created by InitRepo (`scn init`)",
            ));
        }
        let submitter = principal(&request)?.to_owned();
        let repo = cs.repo.clone();
        // `main` is the request's base, read while the changeset is stored.
        let (created, main) = tokio::join!(
            create(
                &*self.meta,
                &*self.objects,
                std::slice::from_ref(&cs),
                false
            ),
            self.meta.get_ref(&repo, "main"),
        );
        let [(id, _)] = created?.try_into().expect("one changeset in, one out");
        let main = main
            .map_err(store_status)?
            .ok_or_else(|| Status::not_found(format!("ref \"main\" in {repo:?}")))?;
        let cr = self
            .meta
            .submit_change_request(&repo, id, main.target, &submitter)
            .await
            .map_err(store_status)?;
        self.workers.wake(&repo);
        let cr = self
            .workers
            .wait(&repo, cr.id)
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::internal(format!("change request {} vanished", cr.id)))?;
        Ok(Response::new(change_request(cr)))
    }

    async fn get_change_request(
        &self,
        request: Request<GetChangeRequestRequest>,
    ) -> Result<Response<ChangeRequest>, Status> {
        let _rpc = span("GetChangeRequest");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
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
        let _rpc = span("ListChangeRequests");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
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
        let _rpc = span("FindLanding");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
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

    async fn grant(
        &self,
        request: Request<GrantRequest>,
    ) -> Result<Response<GrantResponse>, Status> {
        crate::grants::grant(&*self.meta, request).await
    }

    async fn revoke(
        &self,
        request: Request<RevokeRequest>,
    ) -> Result<Response<RevokeResponse>, Status> {
        crate::grants::revoke(&*self.meta, request).await
    }

    async fn list_grants(
        &self,
        request: Request<ListGrantsRequest>,
    ) -> Result<Response<ListGrantsResponse>, Status> {
        crate::grants::list_grants(&*self.meta, request).await
    }

    async fn who_am_i(
        &self,
        request: Request<WhoAmIRequest>,
    ) -> Result<Response<WhoAmIResponse>, Status> {
        crate::grants::who_am_i(&*self.meta, request).await
    }
}
