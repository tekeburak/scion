//! Import a git history into the stores.

use std::collections::HashMap;

use bytes::Bytes;
use scion_store::{MetadataStore, ObjectStore};
use scion_types::ChangesetId;

use crate::{GitRepo, IngestError};

/// The only ref an import creates or moves.
pub const MAIN_REF: &str = "main";

/// Import every commit reachable from `rev` in `git` into repository `repo`,
/// parents first, then create `main` at the tip or fast-forward it there.
/// Returns the tip's changeset.
pub async fn import(
    git: &GitRepo,
    rev: &str,
    repo: &str,
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
) -> Result<ChangesetId, IngestError> {
    let commits = git.commits(rev)?;
    let mut imported: HashMap<_, ChangesetId> = HashMap::with_capacity(commits.len());
    for commit in &commits {
        let mapped = git.changeset(commit.id, repo, &imported)?;
        for (_, blob) in &mapped.blobs {
            let data = git.blob(*blob)?;
            objects
                .put_blob(Bytes::from(data))
                .await
                .map_err(|e| IngestError::Store(commit.id, e.to_string()))?;
        }
        let (id, _) = scion_server::create_changeset(meta, objects, &mapped.changeset)
            .await
            .map_err(|e| IngestError::Store(commit.id, e.message().to_owned()))?;
        imported.insert(commit.id, id);
    }

    let tip_commit = commits
        .last()
        .expect("a revision names at least one commit")
        .id;
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
    Ok(tip)
}
