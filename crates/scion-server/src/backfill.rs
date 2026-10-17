//! Derive a type for a repository's whole history ahead of the first request.

use scion_derived::{Derivable, Deriver, PathHistory, RootTree, SkipList};
use scion_store::{MetadataStore, ObjectStore};

/// The types [`backfill`] derives: those a created changeset gets eagerly.
/// Blame is derived on demand, per file, from the file's own version chain.
pub const BACKFILL_TYPES: [&str; 3] = [RootTree::NAME, PathHistory::NAME, SkipList::NAME];

/// Derive `ty` at its current version for every changeset reachable from
/// `repo`'s refs that does not have it yet, parents before children.
///
/// Each value is stored as soon as it is derived and the walk stops at stored
/// values, so an interrupted run resumes where it stopped and a finished one
/// is a no-op. Returns the number of refs walked.
pub async fn backfill(
    repo: &str,
    ty: &str,
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
) -> Result<usize, String> {
    if !BACKFILL_TYPES.contains(&ty) {
        return Err(format!(
            "unknown derived type {ty:?}; backfill derives {}",
            BACKFILL_TYPES.join(", ")
        ));
    }
    let refs = meta.list_refs(repo).await.map_err(|e| e.to_string())?;
    if refs.is_empty() {
        return Err(format!("repository {repo:?} has no refs"));
    }
    let ctx = Deriver::new(repo, meta, objects);
    for (name, state) in &refs {
        let derived = match ty {
            RootTree::NAME => ctx.derive::<RootTree>(state.target).await.map(drop),
            PathHistory::NAME => ctx.derive::<PathHistory>(state.target).await.map(drop),
            SkipList::NAME => ctx.derive::<SkipList>(state.target).await.map(drop),
            _ => unreachable!("checked above"),
        };
        derived.map_err(|e| format!("{ty} at {name} ({}): {e}", state.target))?;
    }
    Ok(refs.len())
}
