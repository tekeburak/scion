//! Import a git history into the stores.

use std::collections::HashMap;

use bytes::Bytes;
use scion_store::{MetadataStore, ObjectStore};
use scion_types::ChangesetId;

use crate::{GitRepo, IngestError, ObjectId};

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
/// every commit already recorded and creates exactly the rest.
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

    let mut created = 0;
    for commit in &commits {
        if imported.contains_key(&commit.id) {
            continue;
        }
        let mapped = git.changeset(commit.id, repo, &imported)?;
        for (_, blob) in &mapped.blobs {
            let data = git.blob(*blob)?;
            objects
                .put_blob(Bytes::from(data))
                .await
                .map_err(store_err(commit.id))?;
        }
        let (id, _) = scion_server::create_changeset(meta, objects, &mapped.changeset)
            .await
            .map_err(|e| IngestError::Store(commit.id, e.message().to_owned()))?;
        meta.put_ingest_checkpoint(repo, commit.id.as_bytes(), id)
            .await
            .map_err(store_err(commit.id))?;
        imported.insert(commit.id, id);
        created += 1;
    }

    let tip = imported[&tip_commit];
    let current = meta
        .get_ref(repo, MAIN_REF)
        .await
        .map_err(|e| IngestError::Ref(tip_commit, e.to_string()))?;
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
