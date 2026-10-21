//! Import a git history into the stores.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use scion_proto::trace::{span, timed};
use scion_store::{MetadataStore, ObjectStore};
use scion_types::{BlobId, Changeset, ChangesetId};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;

use crate::{GIT_SHA, GitRepo, IngestError, Mapped, ObjectId, Read};

/// Most blob uploads in flight at once for one commit.
const UPLOAD_CONCURRENCY: usize = 16;
/// Most commits whose blobs upload while earlier ones are being created.
const UPLOAD_AHEAD: usize = 16;
/// Most mapped commits waiting for their upload to start, and most read
/// commits waiting to be mapped.
const MAP_AHEAD: usize = 8;
/// Threads reading commits from git.
const MAP_THREADS: usize = 2;
/// Most changesets created in one batch.
const BATCH: usize = 64;

/// The only ref an import creates or moves.
pub const MAIN_REF: &str = "main";

/// What an [`import`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Imported {
    /// The changeset of the imported revision; `main` points at it.
    pub tip: ChangesetId,
    /// Changesets created by this run. Commits recorded in
    /// `ingest_checkpoints` by an earlier run are skipped.
    pub created: usize,
}

/// Import every commit reachable from `rev` in `git` into repository `repo`,
/// parents first, then create `main` at the tip or fast-forward it there.
///
/// Each created batch of changesets is recorded in `ingest_checkpoints` right
/// after its derived data is stored, so an interrupted import resumes where it
/// stopped: a run skips every commit already recorded and creates exactly the
/// rest. Re-running
/// against an updated repository therefore imports only the new commits.
/// If `main` already points at a commit that `rev` does not contain (the
/// upstream history was rewritten), nothing is written.
///
/// `repo` must have been created by `scn init`; while `main` is still at the
/// empty [`Changeset::repository_root`], the import replaces it.
pub async fn import(
    git: &GitRepo,
    rev: &str,
    repo: &str,
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
) -> Result<Imported, IngestError> {
    let commits = git.commits(rev)?;
    let tip_commit = commits
        .last()
        .expect("a revision names at least one commit")
        .id;
    let store_err =
        |id: ObjectId| move |e: scion_store::StoreError| IngestError::Store(id, e.to_string());

    // Refuse an unknown repository and a rewritten upstream before writing
    // anything: `main` must be at the empty root `scn init` created or at a
    // commit this revision still contains.
    let current = meta
        .get_ref(repo, MAIN_REF)
        .await
        .map_err(|e| IngestError::Ref(tip_commit, e.to_string()))?
        .ok_or_else(|| IngestError::NoRepository(repo.to_owned()))?;
    let empty = current.target == Changeset::repository_root(repo).id();
    if !empty {
        let main_sha = meta
            .get_changeset(repo, current.target)
            .await
            .map_err(store_err(tip_commit))?
            .and_then(|cs| cs.extra.get(GIT_SHA).cloned())
            .and_then(|sha| ObjectId::try_from(sha.as_slice()).ok());
        if !main_sha.is_some_and(|sha| commits.iter().any(|c| c.id == sha)) {
            return Err(IngestError::Diverged {
                main: main_sha.map_or_else(
                    || format!("changeset {} (not imported from git)", current.target),
                    |sha| format!("git commit {sha}"),
                ),
                rev: rev.to_owned(),
            });
        }
    }

    let mut imported: HashMap<ObjectId, ChangesetId> = HashMap::with_capacity(commits.len());
    for (sha, cs) in meta
        .ingest_checkpoints(repo)
        .await
        .map_err(store_err(tip_commit))?
    {
        if let Ok(id) = ObjectId::try_from(sha.as_slice()) {
            imported.insert(id, cs);
        }
    }

    // Three stages, so neither git nor the stores wait on each other: a
    // blocking thread maps commits (a changeset's id is its hash, so it
    // needs no store to name the parents of the next one), up to
    // `UPLOAD_AHEAD` commits upload their blobs concurrently, and the
    // uploaded ones are created in order, in batches of whatever is ready,
    // through the shared create path.
    let pending: Vec<ObjectId> = commits
        .iter()
        .map(|c| c.id)
        .filter(|id| !imported.contains_key(id))
        .collect();
    let (mapper, rx) = spawn_mapper(git.git_dir(), repo, pending, imported.clone());

    let (uploaded_tx, uploaded_rx) = mpsc::channel(BATCH);
    let upload_all = async move {
        let mut ready = ReceiverStream::new(rx)
            .map(|mapped| async move {
                let (commit, Mapped { changeset, blobs }) = mapped?;
                upload(objects, commit, blobs).await?;
                Ok::<_, IngestError>((commit, changeset))
            })
            .buffered(UPLOAD_AHEAD);
        while let Some(uploaded) = ready.try_next().await? {
            if uploaded_tx.send(uploaded).await.is_err() {
                break;
            }
        }
        // Dropping `ready` closes the mapper's channel, which stops it.
        Ok::<_, IngestError>(())
    };
    let create_all = create(uploaded_rx, repo, meta, objects, &mut imported);
    // A failed stage drops its end of the channel between them, which ends
    // the other one.
    let (uploaded, created) = tokio::join!(upload_all, create_all);
    mapper.await.expect("the mapping thread does not panic");
    uploaded?;
    let created = created?;

    let tip = imported[&tip_commit];
    if empty {
        // The imported history does not descend from the empty root, so this
        // is a plain compare-and-swap rather than a fast-forward.
        meta.move_ref(repo, MAIN_REF, current.version, tip)
            .await
            .map_err(|e| IngestError::Ref(tip_commit, e.to_string()))?;
    } else if current.target != tip {
        scion_server::move_ref(meta, objects, repo, MAIN_REF, tip, Some(current.version))
            .await
            .map_err(|e| IngestError::Ref(tip_commit, e.message().to_owned()))?;
    }
    Ok(Imported { tip, created })
}

/// Create the uploaded commits from `uploaded`, in order, in batches of
/// whatever is ready, and record each batch's checkpoints once it is stored.
/// Adds each created changeset to `imported`; returns how many there were.
async fn create(
    uploaded: mpsc::Receiver<(ObjectId, Changeset)>,
    repo: &str,
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    imported: &mut HashMap<ObjectId, ChangesetId>,
) -> Result<usize, IngestError> {
    let mut created = 0;
    let mut batches = ReceiverStream::new(uploaded).ready_chunks(BATCH);
    while let Some(batch) = batches.next().await {
        let (commits, changesets): (Vec<ObjectId>, Vec<_>) = batch.into_iter().unzip();
        let last = *commits.last().expect("a chunk is never empty");
        let ids = scion_server::create_changesets(meta, objects, &changesets)
            .await
            .map_err(|e| IngestError::Store(last, e.message().to_owned()))?;
        let done: Vec<_> = commits
            .iter()
            .zip(&ids)
            .map(|(commit, (id, _))| (commit.as_bytes().to_vec(), *id))
            .collect();
        timed("checkpoint", meta.put_ingest_checkpoints(repo, &done))
            .await
            .map_err(|e| IngestError::Store(last, e.to_string()))?;
        for (commit, (id, _)) in commits.into_iter().zip(ids) {
            imported.insert(commit, id);
        }
        created += done.len();
    }
    Ok(created)
}

/// Upload the blobs of `commit` the store does not have yet: one presence
/// check, then the missing ones concurrently.
async fn upload(
    objects: &dyn ObjectStore,
    commit: ObjectId,
    blobs: BTreeMap<BlobId, Bytes>,
) -> Result<(), IngestError> {
    let store_err = |e: scion_store::StoreError| IngestError::Store(commit, e.to_string());
    let ids: Vec<_> = blobs.keys().copied().collect();
    let present = timed("has_blobs", objects.has_blobs(&ids))
        .await
        .map_err(store_err)?;
    let _put = span("put");
    stream::iter(
        blobs
            .into_values()
            .zip(present)
            .filter(|(_, present)| !present)
            .map(|(data, _)| objects.put_blob(data)),
    )
    .buffer_unordered(UPLOAD_CONCURRENCY)
    .try_for_each(|_| async { Ok(()) })
    .await
    .map_err(store_err)
}

type MapResult = Result<(ObjectId, Mapped), IngestError>;

/// Map `pending` commits, in order. `MAP_THREADS` blocking threads read
/// them from git, commit `i` on thread `i % MAP_THREADS`; one more resolves
/// each in order against the changesets before it. `ids` holds the
/// changesets of the commits already imported; each mapped commit's id is
/// added as it goes. Mapping stops at the first error, which is sent, or
/// when the receiver is dropped.
fn spawn_mapper(
    git_dir: &Path,
    repo: &str,
    pending: Vec<ObjectId>,
    mut ids: HashMap<ObjectId, ChangesetId>,
) -> (JoinHandle<()>, mpsc::Receiver<MapResult>) {
    let (tx, rx) = mpsc::channel(MAP_AHEAD);
    let readers: Vec<_> = (0..MAP_THREADS)
        .map(|k| {
            let commits: Vec<_> = pending
                .iter()
                .copied()
                .skip(k)
                .step_by(MAP_THREADS)
                .collect();
            spawn_reader(git_dir, repo, commits)
        })
        .collect();
    let handle = tokio::task::spawn_blocking(move || {
        for i in 0..pending.len() {
            // A reader that stopped early has sent its error, which ends the
            // loop before its channel is read again.
            let Ok(read) = readers[i % MAP_THREADS].recv() else {
                return;
            };
            let mapped = read.and_then(|(commit, read)| {
                let mapped = read.resolve(&ids)?;
                ids.insert(commit, mapped.changeset.id());
                Ok((commit, mapped))
            });
            let failed = mapped.is_err();
            if tx.blocking_send(mapped).is_err() || failed {
                return;
            }
        }
    });
    (handle, rx)
}

/// Read `commits` from git, in order, on a blocking thread, which stops at the
/// first error, which it sends, or when the receiver is dropped.
fn spawn_reader(
    git_dir: &Path,
    repo: &str,
    commits: Vec<ObjectId>,
) -> std::sync::mpsc::Receiver<Result<(ObjectId, Read), IngestError>> {
    let (tx, rx) = std::sync::mpsc::sync_channel(MAP_AHEAD / MAP_THREADS);
    // A repository handle stays on its thread; each reader opens its own.
    let git_dir = git_dir.to_owned();
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || {
        let git = match GitRepo::open(&git_dir) {
            Ok(git) => git,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        for commit in commits {
            let map = span("map");
            let read = git.read(commit, &repo).map(|read| (commit, read));
            drop(map);
            let failed = read.is_err();
            if tx.send(read).is_err() || failed {
                return;
            }
        }
    });
    rx
}
