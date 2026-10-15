//! Import a git history into the stores.

use std::collections::HashMap;

use scion_store::{MetadataStore, ObjectStore};
use scion_types::ChangesetId;
use tokio::sync::mpsc;

use crate::staged::{QUEUE_DEPTH, Shared, Staged, write_behind};
use crate::{GIT_SHA, GitRepo, IngestError, Mapped, ObjectId};

/// Commits mapped ahead of the one being created.
const MAP_AHEAD: usize = 16;

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

    // Three stages run at once: a thread maps commits ahead of creation,
    // changesets are created one by one through the shared create path over
    // the staged stores, and the writer stores what that queues.
    let (to_create, mut ready) = mpsc::channel(MAP_AHEAD);
    let mapper = {
        let (dir, repo, mut ids) = (git.dir(), repo.to_owned(), imported.clone());
        tokio::task::spawn_blocking(move || -> Result<(), IngestError> {
            // A repository handle stays on the thread that opened it.
            let git = GitRepo::open(&dir)?;
            for commit in commits {
                if ids.contains_key(&commit.id) {
                    continue;
                }
                let m = git.changeset(commit.id, &repo, &ids)?;
                ids.insert(commit.id, m.changeset.id());
                if to_create.blocking_send((commit.id, m)).is_err() {
                    break;
                }
            }
            Ok(())
        })
    };
    let cache = Shared::default();
    let (queue, queued) = mpsc::channel(QUEUE_DEPTH);
    let writer = write_behind(meta, objects, &cache, queued);
    let create = async {
        let staged = Staged::new(meta, objects, &cache, queue);
        let mut created = 0;
        while let Some((sha, Mapped { changeset, blobs })) = ready.recv().await {
            staged.begin(sha);
            for (id, data) in blobs {
                staged.put_file(id, data).await.map_err(store_err(sha))?;
            }
            let (id, _) = scion_server::create_changeset(&staged, &staged, &changeset)
                .await
                .map_err(|e| IngestError::Store(sha, e.message().to_owned()))?;
            staged
                .put_ingest_checkpoint(repo, sha.as_bytes(), id)
                .await
                .map_err(store_err(sha))?;
            imported.insert(sha, id);
            created += 1;
        }
        // Closes the queue: the writer stores what is left and returns.
        drop(staged);
        mapper.await.expect("the mapping thread does not panic")?;
        Ok(created)
    };
    let (created, ()) = tokio::try_join!(create, writer)?;

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
