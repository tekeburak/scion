//! `scion-ingest`: the git → Scion importer (roadmap Phase 6).
//!
//! This step only reads history: [`commits`] lists every commit reachable
//! from a revision, parents before children, which is the order changesets
//! must be created in (a changeset's parents must already exist).

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::path::Path;

pub use gix::ObjectId;

/// Failure reading a git repository.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("cannot open git repository {0}: {1}")]
    Open(String, String),
    #[error("cannot resolve {0:?}: {1}")]
    Rev(String, String),
    #[error("git object {0}: {1}")]
    Object(ObjectId, String),
}

/// A commit and its parents, in git's order (first parent first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommit {
    pub id: ObjectId,
    pub parents: Vec<ObjectId>,
}

/// Every commit reachable from `rev` in the repository at `git_dir`, each
/// after all of its parents. Ties are broken by the order of the parent
/// lists, so the result is the same on every run.
pub fn commits(git_dir: &Path, rev: &str) -> Result<Vec<GitCommit>, IngestError> {
    let repo = gix::open(git_dir)
        .map_err(|e| IngestError::Open(git_dir.display().to_string(), e.to_string()))?;
    let tip = repo
        .rev_parse_single(rev)
        .map_err(|e| IngestError::Rev(rev.to_owned(), e.to_string()))?
        .object()
        .and_then(gix::Object::peel_to_commit)
        .map_err(|e| IngestError::Rev(rev.to_owned(), e.to_string()))?
        .id;

    let mut parents: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let mut stack = vec![tip];
    while let Some(id) = stack.pop() {
        if parents.contains_key(&id) {
            continue;
        }
        let commit = repo
            .find_commit(id)
            .map_err(|e| IngestError::Object(id, e.to_string()))?;
        let ps: Vec<ObjectId> = commit.parent_ids().map(gix::Id::detach).collect();
        stack.extend(ps.iter().copied());
        parents.insert(id, ps);
    }

    // Iterative post-order DFS from the tip: a commit is emitted once all
    // of its parents have been.
    let mut out = Vec::with_capacity(parents.len());
    let mut done: HashSet<ObjectId> = HashSet::with_capacity(parents.len());
    let mut stack: Vec<(ObjectId, usize)> = vec![(tip, 0)];
    while let Some((id, next)) = stack.pop() {
        let ps = &parents[&id];
        if let Some(&p) = ps.get(next) {
            stack.push((id, next + 1));
            if !done.contains(&p) {
                stack.push((p, 0));
            }
        } else if done.insert(id) {
            out.push(GitCommit {
                id,
                parents: ps.clone(),
            });
        }
    }
    Ok(out)
}
