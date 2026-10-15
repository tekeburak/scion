//! Import a git history into the stores.

use std::collections::HashMap;
use std::num::NonZero;
use std::path::Path;

use scion_store::{MetadataStore, ObjectStore};
use scion_types::ChangesetId;
use tokio::sync::mpsc;

use crate::stores::Stores;
use crate::{GIT_SHA, GitRepo, IngestError, Mapped, ObjectId, Unresolved};

/// Commits each reader thread reads ahead of the import.
const READ_AHEAD: usize = 8;

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
///
/// Commits are read from git on one thread per core, ahead of the import,
/// and every changeset goes through the same create path as the
/// `CreateChangeset` RPC, one at a time, parents first. The stores are
/// written behind it, concurrently but in an order that never lets metadata
/// name a missing object or parent (see `stores.rs`), so a commit costs
/// about one round trip of each store, not one per object it writes.
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
    let todo: Vec<ObjectId> = commits
        .iter()
        .map(|c| c.id)
        .filter(|id| !imported.contains_key(id))
        .collect();
    let mut reads = read_ahead(git.repo.git_dir(), &todo, repo);

    let (stores, queue) = Stores::new(meta, objects);
    let produce = async {
        let result = async {
            for (i, id) in todo.iter().enumerate() {
                let n = reads.len();
                let read = reads[i % n]
                    .recv()
                    .await
                    .ok_or_else(|| IngestError::Object(*id, "the reader stopped".into()))??;
                let Mapped { changeset, blobs } = read.resolve(&imported)?;
                for data in blobs.into_values() {
                    stores.put_blob(data).await.map_err(store_err(*id))?;
                }
                let (cs, _) = scion_server::create_changeset(&stores, &stores, &changeset)
                    .await
                    .map_err(|e| IngestError::Store(*id, e.message().to_owned()))?;
                stores
                    .put_ingest_checkpoint(repo, id.as_bytes(), cs)
                    .await
                    .map_err(store_err(*id))?;
                imported.insert(*id, cs);
            }

            let tip = imported[&tip_commit];
            if current.as_ref().is_none_or(|c| c.target != tip) {
                scion_server::move_ref(
                    &stores,
                    &stores,
                    repo,
                    MAIN_REF,
                    tip,
                    current.map(|c| c.version),
                )
                .await
                .map_err(|e| IngestError::Ref(tip_commit, e.message().to_owned()))?;
            }
            Ok(Imported {
                tip,
                created: todo.len(),
            })
        }
        .await;
        stores.close();
        result
    };
    // A failed write stops the import; report it rather than the import
    // noticing that the writer stopped.
    match tokio::join!(produce, stores.write(queue)) {
        (_, Err(e)) => Err(IngestError::Write(e.to_string())),
        (result, Ok(())) => result,
    }
}

/// Read `commits` on worker threads, each with its own handle on the
/// repository at `git_dir`. Commit `i` arrives on channel `i % n`, so taking
/// from the channels in turn yields them in order. A reader stops after an
/// error or when its channel is dropped.
fn read_ahead(
    git_dir: &Path,
    commits: &[ObjectId],
    repo: &str,
) -> Vec<mpsc::Receiver<Result<Unresolved, IngestError>>> {
    let n = std::thread::available_parallelism()
        .map_or(1, NonZero::get)
        .clamp(1, commits.len().max(1));
    (0..n)
        .map(|k| {
            let (tx, rx) = mpsc::channel(READ_AHEAD);
            let mine: Vec<ObjectId> = commits.iter().skip(k).step_by(n).copied().collect();
            let (dir, repo) = (git_dir.to_owned(), repo.to_owned());
            std::thread::spawn(move || {
                let git = match GitRepo::open(&dir) {
                    Ok(git) => git,
                    Err(e) => {
                        let _ = tx.blocking_send(Err(e));
                        return;
                    }
                };
                for id in mine {
                    let read = git.read(id, &repo);
                    let failed = read.is_err();
                    if tx.blocking_send(read).is_err() || failed {
                        return;
                    }
                }
            });
            rx
        })
        .collect()
}
