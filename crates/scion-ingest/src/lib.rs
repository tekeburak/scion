//! `scion-ingest`: the git → Scion importer (roadmap Phase 6).
//!
//! [`GitRepo::commits`] lists every commit reachable from a revision,
//! parents before children, which is the order changesets must be created in
//! (a changeset's parents must already exist). [`GitRepo::changeset`] maps one
//! commit to a [`Changeset`]:
//!
//! * file changes come from the tree diff against the **first** parent (the
//!   empty tree for a root commit); other parents only appear in `parents`;
//! * an added file renamed or copied from a file the same commit deletes or
//!   modifies (same content, or at least half its lines shared) records it
//!   as `copy_from`, at the first parent's changeset, so blame follows it;
//! * modes map to [`FileType`]; submodules, paths that are not valid
//!   [`RepoPath`]s and blobs over the per-blob limit are rejected;
//! * author and committer keep their own dates and UTC offsets; names, emails
//!   and the message are taken as UTF-8 (invalid bytes become U+FFFD), and the
//!   commit's raw 20-byte sha goes into `extra["git-sha"]`, so the original
//!   stays recoverable.
//!
//! [`import`] writes the result to the stores through the same path as the
//! `CreateChangeset` RPC, then points `main` at the imported tip.

#![forbid(unsafe_code)]

mod import;

pub use import::{Imported, MAIN_REF, import};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub use gix::ObjectId;
use gix::objs::tree::{EntryKind, EntryMode};
use scion_types::{
    BlobId, Changeset, ChangesetId, FileChange, FileType, Identity, RepoPath, Timestamp,
};

/// Largest blob a changeset may reference: the server's per-blob limit.
pub const MAX_BLOB_BYTES: usize = scion_server::MAX_MESSAGE_BYTES;

/// Key of the commit's sha in [`Changeset::extra`].
pub const GIT_SHA: &str = "git-sha";

/// Failure reading a git repository or mapping one of its commits.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("cannot open git repository {0}: {1}")]
    Open(String, String),
    #[error("cannot resolve {0:?}: {1}")]
    Rev(String, String),
    #[error("git object {0}: {1}")]
    Object(ObjectId, String),
    #[error("commit {commit}: {path:?}: {reason}")]
    Unsupported {
        commit: ObjectId,
        path: String,
        reason: &'static str,
    },
    #[error("commit {commit}: parent {parent} has not been imported")]
    MissingParent { commit: ObjectId, parent: ObjectId },
    #[error("storing commit {0}: {1}")]
    Store(ObjectId, String),
    #[error("moving {MAIN_REF} to commit {0}: {1}")]
    Ref(ObjectId, String),
    #[error(
        "{MAIN_REF} is at {main}, which {rev:?} does not contain: the upstream history was \
         rewritten, nothing was imported"
    )]
    Diverged { main: String, rev: String },
}

/// A commit and its parents, in git's order (first parent first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommit {
    pub id: ObjectId,
    pub parents: Vec<ObjectId>,
}

/// One commit as Scion sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped {
    pub changeset: Changeset,
    /// The git blob behind each new or changed file, to upload before the
    /// changeset is created.
    pub blobs: Vec<(BlobId, ObjectId)>,
}

/// A git repository opened for import.
#[derive(Debug)]
pub struct GitRepo {
    repo: gix::Repository,
}

/// One entry of a git tree.
type Entries = BTreeMap<Vec<u8>, (EntryMode, ObjectId)>;

/// A file's kind and git blob.
type File = (FileType, ObjectId);

/// A changed file: its raw path, its old side (`None` if added) and its new
/// side (`None` if deleted).
type Change = (Vec<u8>, Option<File>, Option<File>);

/// Most (added file, source file) pairs compared by content in one commit;
/// past this only exact renames and copies are found, like git's rename limit.
const MAX_INEXACT_PAIRS: usize = 10_000;

impl GitRepo {
    /// Open the repository at `git_dir` (a work tree or a bare repository).
    pub fn open(git_dir: &Path) -> Result<Self, IngestError> {
        let repo = gix::open(git_dir)
            .map_err(|e| IngestError::Open(git_dir.display().to_string(), e.to_string()))?;
        Ok(Self { repo })
    }

    /// Every commit reachable from `rev`, each after all of its parents.
    /// Ties are broken by the order of the parent lists, so the result is the
    /// same on every run.
    pub fn commits(&self, rev: &str) -> Result<Vec<GitCommit>, IngestError> {
        let tip = self
            .repo
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
            let commit = self
                .repo
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

    /// Map commit `id` of this repository to a changeset of `repo`.
    /// `imported` must hold the changeset of every parent.
    pub fn changeset(
        &self,
        id: ObjectId,
        repo: &str,
        imported: &HashMap<ObjectId, ChangesetId>,
    ) -> Result<Mapped, IngestError> {
        let obj_err = |e: &dyn std::fmt::Display| IngestError::Object(id, e.to_string());
        let commit = self.repo.find_commit(id).map_err(|e| obj_err(&e))?;
        let parent_ids: Vec<ObjectId> = commit.parent_ids().map(gix::Id::detach).collect();
        let c = commit.decode().map_err(|e| obj_err(&e))?;

        let parents = parent_ids
            .iter()
            .map(|p| {
                imported.get(p).copied().ok_or(IngestError::MissingParent {
                    commit: id,
                    parent: *p,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let old_tree = match parent_ids.first() {
            Some(p) => Some(self.tree_of(*p)?),
            None => None,
        };
        let mut changes = Vec::new();
        self.diff(id, &[], old_tree, Some(c.tree()), &mut changes)?;

        let sources = match parents.first() {
            Some(_) => self.copy_sources(&changes)?,
            None => HashMap::new(),
        };

        let mut file_changes = BTreeMap::new();
        let mut blobs = Vec::new();
        for (raw, _, new) in &changes {
            let new = *new;
            let to_path = |raw: &[u8]| {
                std::str::from_utf8(raw)
                    .ok()
                    .and_then(|s| RepoPath::new(s).ok())
                    .ok_or_else(|| IngestError::Unsupported {
                        commit: id,
                        path: String::from_utf8_lossy(raw).into_owned(),
                        reason: "not a valid repository path (UTF-8, no empty, `.` or `..` component)",
                    })
            };
            let copy_from = match (sources.get(raw), parents.first()) {
                (Some(from), Some(parent)) => Some((to_path(from)?, *parent)),
                _ => None,
            };
            let path = to_path(raw)?;
            let change = match new {
                None => FileChange::Deleted,
                Some((kind, blob)) => {
                    let data = self.blob(blob)?;
                    if data.len() > MAX_BLOB_BYTES {
                        return Err(IngestError::Unsupported {
                            commit: id,
                            path: path.to_string(),
                            reason: "blob is larger than the per-blob limit",
                        });
                    }
                    let content = BlobId::hash(&data);
                    blobs.push((content, blob));
                    FileChange::Tracked {
                        content,
                        kind,
                        size: data.len() as u64,
                        copy_from,
                    }
                }
            };
            file_changes.insert(path, change);
        }

        let author = c.author().map_err(|e| obj_err(&e))?;
        let committer = c.committer().map_err(|e| obj_err(&e))?;
        let changeset = Changeset {
            repo: repo.to_owned(),
            parents,
            author: identity(author),
            author_date: timestamp(author.time().map_err(|e| obj_err(&e))?),
            committer: Some(identity(committer)),
            committer_date: Some(timestamp(committer.time().map_err(|e| obj_err(&e))?)),
            message: String::from_utf8_lossy(c.message).into_owned(),
            file_changes,
            extra: BTreeMap::from([(GIT_SHA.to_owned(), id.as_bytes().to_vec())]),
        };
        Ok(Mapped { changeset, blobs })
    }

    /// For each added file, the file it was renamed or copied from, if any:
    /// a file deleted or modified by the same commit (its version in the
    /// first parent) with the same content, or else the most similar one
    /// sharing at least half its lines. Paths are raw bytes as in `changes`.
    fn copy_sources(&self, changes: &[Change]) -> Result<HashMap<Vec<u8>, Vec<u8>>, IngestError> {
        let sources: Vec<(&Vec<u8>, ObjectId)> = changes
            .iter()
            .filter_map(|(path, old, new)| match (old, new) {
                (Some((_, old)), new) if new.is_none_or(|(_, n)| n != *old) => Some((path, *old)),
                _ => None,
            })
            .collect();
        let added: Vec<(&Vec<u8>, ObjectId)> = changes
            .iter()
            .filter_map(|(path, old, new)| match (old, new) {
                (None, Some((_, new))) => Some((path, *new)),
                _ => None,
            })
            .collect();
        let mut out = HashMap::new();
        if sources.is_empty() || added.is_empty() {
            return Ok(out);
        }
        let compare = sources.len() * added.len() <= MAX_INEXACT_PAIRS;
        let mut source_data: HashMap<ObjectId, Vec<u8>> = HashMap::new();
        for (path, blob) in added {
            if let Some((from, _)) = sources.iter().find(|(_, s)| *s == blob) {
                out.insert(path.clone(), (*from).clone());
                continue;
            }
            if !compare {
                continue;
            }
            let data = self.blob(blob)?;
            let mut best: Option<(u64, &Vec<u8>)> = None;
            for (from, source) in &sources {
                if !source_data.contains_key(source) {
                    source_data.insert(*source, self.blob(*source)?);
                }
                let score = similarity(&source_data[source], &data);
                if score >= 50 && best.is_none_or(|(b, _)| score > b) {
                    best = Some((score, from));
                }
            }
            if let Some((_, from)) = best {
                out.insert(path.clone(), from.clone());
            }
        }
        Ok(out)
    }

    /// The contents of git blob `id`.
    pub fn blob(&self, id: ObjectId) -> Result<Vec<u8>, IngestError> {
        Ok(self
            .repo
            .find_blob(id)
            .map_err(|e| IngestError::Object(id, e.to_string()))?
            .take_data())
    }

    fn tree_of(&self, commit: ObjectId) -> Result<ObjectId, IngestError> {
        self.repo
            .find_commit(commit)
            .and_then(|c| c.tree_id().map(gix::Id::detach))
            .map_err(|e| IngestError::Object(commit, e.to_string()))
    }

    fn entries(&self, tree: Option<ObjectId>) -> Result<Entries, IngestError> {
        let Some(id) = tree else {
            return Ok(Entries::new());
        };
        let tree = self
            .repo
            .find_tree(id)
            .map_err(|e| IngestError::Object(id, e.to_string()))?;
        let decoded = tree
            .decode()
            .map_err(|e| IngestError::Object(id, e.to_string()))?;
        Ok(decoded
            .entries
            .iter()
            .map(|e| (e.filename.to_vec(), (e.mode, e.oid.to_owned())))
            .collect())
    }

    /// Append to `out` every file under `prefix` that differs between trees
    /// `old` and `new` (`None` is the empty tree), with both sides.
    fn diff(
        &self,
        commit: ObjectId,
        prefix: &[u8],
        old: Option<ObjectId>,
        new: Option<ObjectId>,
        out: &mut Vec<Change>,
    ) -> Result<(), IngestError> {
        let old = self.entries(old)?;
        let new = self.entries(new)?;
        let names: std::collections::BTreeSet<&Vec<u8>> = old.keys().chain(new.keys()).collect();
        for name in names {
            let (o, n) = (old.get(name), new.get(name));
            if o == n {
                continue;
            }
            let mut path = prefix.to_vec();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(name);
            let as_file = |side: Option<&(EntryMode, ObjectId)>| -> Result<_, IngestError> {
                match side {
                    Some((mode, id)) if !mode.is_tree() => {
                        Ok(Some((file_type(commit, &path, *mode)?, *id)))
                    }
                    _ => Ok(None),
                }
            };
            let as_tree = |side: Option<&(EntryMode, ObjectId)>| match side {
                Some((mode, id)) if mode.is_tree() => Some(*id),
                _ => None,
            };
            let (old_file, new_file) = (as_file(o)?, as_file(n)?);
            let (old_tree, new_tree) = (as_tree(o), as_tree(n));
            if old_tree.is_some() || new_tree.is_some() {
                self.diff(commit, &path, old_tree, new_tree, out)?;
            }
            if new_file.is_some() || old_file.is_some() {
                out.push((path, old_file, new_file));
            }
        }
        Ok(())
    }
}

fn file_type(commit: ObjectId, path: &[u8], mode: EntryMode) -> Result<FileType, IngestError> {
    match mode.kind() {
        EntryKind::Blob => Ok(FileType::Regular),
        EntryKind::BlobExecutable => Ok(FileType::Executable),
        EntryKind::Link => Ok(FileType::Symlink),
        EntryKind::Commit | EntryKind::Tree => Err(IngestError::Unsupported {
            commit,
            path: String::from_utf8_lossy(path).into_owned(),
            reason: "submodules are not supported",
        }),
    }
}

/// Percentage of lines two files share (each line counted as often as it
/// occurs in both), relative to their average length. Empty files are never
/// similar.
fn similarity(a: &[u8], b: &[u8]) -> u64 {
    let lines = |d: &[u8]| d.split_inclusive(|c| *c == b'\n').count() as u64;
    let (la, lb) = (lines(a), lines(b));
    if la == 0 || lb == 0 {
        return 0;
    }
    let mut counts: HashMap<&[u8], u64> = HashMap::new();
    for line in a.split_inclusive(|c| *c == b'\n') {
        *counts.entry(line).or_default() += 1;
    }
    let mut shared = 0;
    for line in b.split_inclusive(|c| *c == b'\n') {
        if let Some(n) = counts.get_mut(line).filter(|n| **n > 0) {
            *n -= 1;
            shared += 1;
        }
    }
    200 * shared / (la + lb)
}

fn identity(sig: gix::actor::SignatureRef<'_>) -> Identity {
    Identity::new(
        String::from_utf8_lossy(sig.name),
        String::from_utf8_lossy(sig.email),
    )
}

fn timestamp(t: gix::date::Time) -> Timestamp {
    Timestamp::new(t.seconds, t.offset / 60)
}
