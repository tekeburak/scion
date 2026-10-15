//! Import a git history into the stores.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use scion_store::{MetadataStore, ObjectStore};
use scion_types::{BlobId, ChangesetId};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;

use crate::{GIT_SHA, GitRepo, IngestError, Mapped, ObjectId};

/// Most blob uploads in flight at once for one commit.
const UPLOAD_CONCURRENCY: usize = 16;
/// Most commits whose blobs upload while earlier ones are being created.
const UPLOAD_AHEAD: usize = 8;
/// Most mapped commits waiting for their upload to start.
const MAP_AHEAD: usize = 8;

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
/// Each created changeset is recorded in `ingest_checkpoints` right after it
/// is stored, so an interrupted import resumes where it stopped: a run skips
/// every commit already recorded and creates exactly the rest. Re-running
/// against an updated repository therefore imports only the new commits.
/// If `main` already points at a commit that `rev` does not contain (the
/// upstream history was rewritten), nothing is written.
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

    // Refuse a rewritten upstream before writing anything: `main` must be at
    // a commit this revision still contains.
    let current = meta
        .get_ref(repo, MAIN_REF)
        .await
        .map_err(|e| IngestError::Ref(tip_commit, e.to_string()))?;
    if let Some(cur) = &current {
        let main_sha = meta
            .get_changeset(repo, cur.target)
            .await
            .map_err(store_err(tip_commit))?
            .and_then(|cs| cs.extra.get(GIT_SHA).cloned())
            .and_then(|sha| ObjectId::try_from(sha.as_slice()).ok());
        if !main_sha.is_some_and(|sha| commits.iter().any(|c| c.id == sha)) {
            return Err(IngestError::Diverged {
                main: main_sha.map_or_else(
                    || format!("changeset {} (not imported from git)", cur.target),
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

    // Three stages, so neither git nor the object store waits on the other:
    // a blocking thread maps commits (a changeset's id is its hash, so it
    // needs no store to name the parents of the next one), up to
    // `UPLOAD_AHEAD` commits upload their blobs concurrently, and changesets
    // are created strictly in order through the shared create path.
    let pending: Vec<ObjectId> = commits
        .iter()
        .map(|c| c.id)
        .filter(|id| !imported.contains_key(id))
        .collect();
    let (mapper, rx) = spawn_mapper(git.git_dir(), repo, pending, imported.clone());

    let mut ready = ReceiverStream::new(rx)
        .map(|mapped| async move {
            let (commit, Mapped { changeset, blobs }) = mapped?;
            upload(objects, commit, blobs).await?;
            Ok::<_, IngestError>((commit, changeset))
        })
        .buffered(UPLOAD_AHEAD);
    let mut created = 0;
    let result = async {
        while let Some((commit, changeset)) = ready.try_next().await? {
            let (id, _) = scion_server::create_changeset(meta, objects, &changeset)
                .await
                .map_err(|e| IngestError::Store(commit, e.message().to_owned()))?;
            meta.put_ingest_checkpoint(repo, commit.as_bytes(), id)
                .await
                .map_err(store_err(commit))?;
            imported.insert(commit, id);
            created += 1;
        }
        Ok::<_, IngestError>(())
    }
    .await;
    // Closing the channel stops the mapper.
    drop(ready);
    mapper.await.expect("the mapping thread does not panic");
    result?;

    let tip = imported[&tip_commit];
    if current.as_ref().is_none_or(|c| c.target != tip) {
        scion_server::move_ref(
            meta,
            objects,
            repo,
            MAIN_REF,
            tip,
            current.map(|c| c.version),
        )
        .await
        .map_err(|e| IngestError::Ref(tip_commit, e.message().to_owned()))?;
    }
    Ok(Imported { tip, created })
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
    let present = objects.has_blobs(&ids).await.map_err(store_err)?;
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

/// Map `pending` commits, in order, on a blocking thread. `ids` holds the
/// changesets of the commits already imported; each mapped commit's id is
/// added as it goes. The thread stops at the first error, which it sends, or
/// when the receiver is dropped.
fn spawn_mapper(
    git_dir: &Path,
    repo: &str,
    pending: Vec<ObjectId>,
    mut ids: HashMap<ObjectId, ChangesetId>,
) -> (JoinHandle<()>, mpsc::Receiver<MapResult>) {
    let (tx, rx) = mpsc::channel(MAP_AHEAD);
    // A repository handle stays on its thread; the mapper opens its own.
    let git_dir = git_dir.to_owned();
    let repo = repo.to_owned();
    let handle = tokio::task::spawn_blocking(move || {
        let git = match GitRepo::open(&git_dir) {
            Ok(git) => git,
            Err(e) => {
                let _ = tx.blocking_send(Err(e));
                return;
            }
        };
        for commit in pending {
            let mapped = git.changeset(commit, &repo, &ids).map(|mapped| {
                ids.insert(commit, mapped.changeset.id());
                (commit, mapped)
            });
            let failed = mapped.is_err();
            if tx.blocking_send(mapped).is_err() || failed {
                return;
            }
        }
    });
    (handle, rx)
}
