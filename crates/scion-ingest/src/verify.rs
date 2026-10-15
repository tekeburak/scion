//! Check an import: every imported commit's tree, flattened, must equal the
//! git tree by path, file kind and content.

use std::collections::{BTreeMap, HashMap};

use scion_derived::{Deriver, RootTree};
use scion_store::{MetadataStore, ObjectStore};
use scion_types::{BlobId, ChangesetId, FileType, TreeEntry, TreeId};

use crate::{GitRepo, IngestError, ObjectId};

/// Every file of a Scion tree: path → kind and content.
async fn flatten(
    objects: &dyn ObjectStore,
    commit: ObjectId,
    root: TreeId,
) -> Result<BTreeMap<String, (FileType, BlobId)>, IngestError> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(String::new(), root)];
    while let Some((prefix, id)) = stack.pop() {
        let tree = objects
            .get_tree(id)
            .await
            .map_err(|e| IngestError::Store(commit, e.to_string()))?
            .ok_or_else(|| IngestError::Store(commit, format!("tree {id} is missing")))?;
        for (name, entry) in tree.entries {
            let path = if prefix.is_empty() {
                name.as_str().to_owned()
            } else {
                format!("{prefix}/{}", name.as_str())
            };
            match entry {
                TreeEntry::Tree { id } => stack.push((path, id)),
                TreeEntry::Blob { id, kind, .. } => {
                    out.insert(path, (kind, id));
                }
            }
        }
    }
    Ok(out)
}

/// Check that changeset `cs` of `repo` holds exactly the files of git
/// `commit`; a mismatch names the first path that differs.
pub async fn verify_commit(
    git: &GitRepo,
    commit: ObjectId,
    repo: &str,
    cs: ChangesetId,
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
) -> Result<(), IngestError> {
    check(git, commit, repo, cs, meta, objects, &mut HashMap::new()).await
}

/// [`verify_commit`], with `hashes` caching git blob -> Scion blob id across
/// commits, since most files are the same from one to the next.
async fn check(
    git: &GitRepo,
    commit: ObjectId,
    repo: &str,
    cs: ChangesetId,
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    hashes: &mut HashMap<ObjectId, BlobId>,
) -> Result<(), IngestError> {
    let root = Deriver::new(repo, meta, objects)
        .derive::<RootTree>(cs)
        .await
        .map_err(|e| IngestError::Store(commit, e.to_string()))?
        .0;
    let scion = flatten(objects, commit, root).await?;

    let mut want = BTreeMap::new();
    for (raw, file) in git.files(commit)? {
        let (kind, blob) = file;
        let id = if let Some(id) = hashes.get(&blob) {
            *id
        } else {
            let id = BlobId::hash(&git.blob(blob)?);
            hashes.insert(blob, id);
            id
        };
        want.insert(String::from_utf8_lossy(&raw).into_owned(), (kind, id));
    }

    let mismatch = |path: &str, what: &'static str| IngestError::Mismatch {
        commit,
        path: path.to_owned(),
        what,
    };
    for (path, got) in &scion {
        match want.get(path) {
            None => return Err(mismatch(path, "in Scion but not in git")),
            Some((kind, _)) if *kind != got.0 => return Err(mismatch(path, "file kind differs")),
            Some((_, id)) if *id != got.1 => return Err(mismatch(path, "content differs")),
            Some(_) => {}
        }
    }
    if let Some(path) = want.keys().find(|p| !scion.contains_key(*p)) {
        return Err(mismatch(path, "in git but not in Scion"));
    }
    Ok(())
}

/// Verify the commits imported into `repo` (from its checkpoints): all of
/// them, or `sample` of them spread evenly. Returns how many were checked.
pub async fn verify(
    git: &GitRepo,
    repo: &str,
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    sample: Option<usize>,
) -> Result<usize, IngestError> {
    let imported: Vec<(ObjectId, ChangesetId)> = meta
        .ingest_checkpoints(repo)
        .await
        .map_err(|e| IngestError::Checkpoints(e.to_string()))?
        .into_iter()
        .filter_map(|(sha, cs)| ObjectId::try_from(sha.as_slice()).ok().map(|id| (id, cs)))
        .collect();
    let step = match sample {
        Some(n) if n > 0 && n < imported.len() => imported.len().div_ceil(n),
        _ => 1,
    };
    let mut hashes = HashMap::new();
    let mut checked = 0;
    for (commit, cs) in imported.iter().step_by(step) {
        check(git, *commit, repo, *cs, meta, objects, &mut hashes).await?;
        checked += 1;
    }
    Ok(checked)
}
