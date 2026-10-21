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
//! [`verify`] checks an import against git, tree by tree.
//!
//! [`import`] writes the result to the stores through the same path as the
//! `CreateChangeset` RPC, then points `main` at the imported tip. The
//! repository must exist (`scn init`); an import never creates one.

#![forbid(unsafe_code)]

mod import;
mod verify;

pub use import::{Imported, MAIN_REF, import};
pub use verify::{verify, verify_commit};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use bytes::Bytes;

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
    #[error("repository {0:?} has no {MAIN_REF}: create it with `scn init <server> {0}` first")]
    NoRepository(String),
    #[error("storing commit {0}: {1}")]
    Store(ObjectId, String),
    #[error("moving {MAIN_REF} to commit {0}: {1}")]
    Ref(ObjectId, String),
    #[error("reading ingest checkpoints: {0}")]
    Checkpoints(String),
    #[error("commit {commit}: {path:?}: {what}")]
    Mismatch {
        commit: ObjectId,
        path: String,
        what: &'static str,
    },
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
    /// The contents of each new or changed file, to upload before the
    /// changeset is created. Held for one commit at a time, so the importer's
    /// memory follows the largest commit, not the history.
    pub blobs: BTreeMap<BlobId, Bytes>,
}

/// One commit read from git: everything [`GitRepo::changeset`] needs except
/// its parents' changeset ids, so commits can be read concurrently and
/// [resolved](Read::resolve) in order.
#[derive(Debug)]
pub struct Read {
    commit: ObjectId,
    parents: Vec<ObjectId>,
    /// `parents` is empty and every `copy_from` names the repository root;
    /// [`Read::resolve`] fills both in.
    changeset: Changeset,
    blobs: BTreeMap<BlobId, Bytes>,
}

impl Read {
    /// The changeset of this commit, given `imported`, which must hold the
    /// changeset of every parent.
    pub fn resolve(self, imported: &HashMap<ObjectId, ChangesetId>) -> Result<Mapped, IngestError> {
        let Self {
            commit,
            parents,
            mut changeset,
            blobs,
        } = self;
        changeset.parents = parents
            .iter()
            .map(|p| {
                imported
                    .get(p)
                    .copied()
                    .ok_or(IngestError::MissingParent { commit, parent: *p })
            })
            .collect::<Result<_, _>>()?;
        if let Some(&first) = changeset.parents.first() {
            for change in changeset.file_changes.values_mut() {
                if let FileChange::Tracked {
                    copy_from: Some((_, at)),
                    ..
                } = change
                {
                    *at = first;
                }
            }
        }
        Ok(Mapped { changeset, blobs })
    }
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

/// Bytes of delta bases kept per open repository, so reading a blob does not
/// re-inflate the whole delta chain of every version before it.
const PACK_CACHE_BYTES: usize = 32 << 20;

impl GitRepo {
    /// Open the repository at `git_dir` (a work tree or a bare repository).
    pub fn open(git_dir: &Path) -> Result<Self, IngestError> {
        let mut repo = gix::open(git_dir)
            .map_err(|e| IngestError::Open(git_dir.display().to_string(), e.to_string()))?;
        repo.objects.set_pack_cache(|| {
            Box::new(gix::odb::pack::cache::lru::MemoryCappedHashmap::new(
                PACK_CACHE_BYTES,
            ))
        });
        Ok(Self { repo })
    }

    /// The repository's git directory.
    pub(crate) fn git_dir(&self) -> &Path {
        self.repo.git_dir()
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
        self.read(id, repo)?.resolve(imported)
    }

    /// Read commit `id` of this repository for a changeset of `repo`: the
    /// tree diff, renames, file contents and identities, which need no other
    /// commit's changeset.
    pub fn read(&self, id: ObjectId, repo: &str) -> Result<Read, IngestError> {
        let obj_err = |e: &dyn std::fmt::Display| IngestError::Object(id, e.to_string());
        let commit = self.repo.find_commit(id).map_err(|e| obj_err(&e))?;
        let parent_ids: Vec<ObjectId> = commit.parent_ids().map(gix::Id::detach).collect();
        let c = commit.decode().map_err(|e| obj_err(&e))?;

        let old_tree = match parent_ids.first() {
            Some(p) => Some(self.tree_of(*p)?),
            None => None,
        };
        let mut changes = Vec::new();
        self.diff(id, &[], old_tree, Some(c.tree()), &mut changes)?;

        let sources = match parent_ids.first() {
            Some(_) => self.copy_sources(&changes)?,
            None => HashMap::new(),
        };
        // `resolve` replaces it with the first parent's changeset.
        let first_parent = Changeset::repository_root(repo).id();

        let mut file_changes = BTreeMap::new();
        let mut blobs = BTreeMap::new();
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
            let copy_from = match sources.get(raw) {
                Some(from) => Some((to_path(from)?, first_parent)),
                None => None,
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
                    let size = data.len() as u64;
                    blobs.insert(content, Bytes::from(data));
                    FileChange::Tracked {
                        content,
                        kind,
                        size,
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
            parents: Vec::new(),
            author: identity(author),
            author_date: timestamp(author.time().map_err(|e| obj_err(&e))?),
            committer: Some(identity(committer)),
            committer_date: Some(timestamp(committer.time().map_err(|e| obj_err(&e))?)),
            message: String::from_utf8_lossy(c.message).into_owned(),
            file_changes,
            extra: BTreeMap::from([(GIT_SHA.to_owned(), id.as_bytes().to_vec())]),
        };
        Ok(Read {
            commit: id,
            parents: parent_ids,
            changeset,
            blobs,
        })
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
        // Each source is read and its lines counted once, not once per pair.
        let inexact = compare
            && added
                .iter()
                .any(|(_, blob)| sources.iter().all(|(_, s)| s != blob));
        let source_data: Vec<Vec<u8>> = if inexact {
            sources
                .iter()
                .map(|(_, source)| self.blob(*source))
                .collect::<Result<_, _>>()?
        } else {
            Vec::new()
        };
        let source_lines: Vec<Lines<'_>> = source_data.iter().map(|d| Lines::of(d)).collect();
        for (path, blob) in added {
            if let Some((from, _)) = sources.iter().find(|(_, s)| *s == blob) {
                out.insert(path.clone(), (*from).clone());
                continue;
            }
            if !compare {
                continue;
            }
            let data = self.blob(blob)?;
            if let Some(from) = most_similar(&Lines::of(&data), &source_lines) {
                out.insert(path.clone(), sources[from].0.clone());
            }
        }
        Ok(out)
    }

    /// Every file of commit `id`'s tree: raw path → kind and git blob.
    pub fn files(&self, id: ObjectId) -> Result<Vec<(Vec<u8>, File)>, IngestError> {
        let tree = self.tree_of(id)?;
        let mut changes = Vec::new();
        self.diff(id, &[], None, Some(tree), &mut changes)?;
        Ok(changes
            .into_iter()
            .filter_map(|(path, _, new)| new.map(|f| (path, f)))
            .collect())
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

/// A file's lines, each with how often it occurs.
struct Lines<'a> {
    counts: HashMap<&'a [u8], u64>,
    total: u64,
}

impl<'a> Lines<'a> {
    fn of(data: &'a [u8]) -> Self {
        let mut counts: HashMap<&[u8], u64> = HashMap::new();
        let mut total = 0;
        for line in data.split_inclusive(|c| *c == b'\n') {
            *counts.entry(line).or_default() += 1;
            total += 1;
        }
        Self { counts, total }
    }
}

/// Percentage of lines two files share (each line counted as often as it
/// occurs in both), relative to their average length. Empty files are never
/// similar.
fn similarity(a: &Lines<'_>, b: &Lines<'_>) -> u64 {
    if a.total == 0 || b.total == 0 {
        return 0;
    }
    let (small, large) = if a.counts.len() <= b.counts.len() {
        (a, b)
    } else {
        (b, a)
    };
    let shared: u64 = small
        .counts
        .iter()
        .map(|(line, n)| (*n).min(large.counts.get(line).copied().unwrap_or(0)))
        .sum();
    200 * shared / (a.total + b.total)
}

/// The index of the first of `sources` most similar to `added`, if any
/// shares at least half its lines. A pair whose line counts alone bound its
/// score below that, or not above the best so far, is not compared.
fn most_similar(added: &Lines<'_>, sources: &[Lines<'_>]) -> Option<usize> {
    let mut best: Option<(u64, usize)> = None;
    for (i, source) in sources.iter().enumerate() {
        let (la, lb) = (source.total, added.total);
        if la == 0 || lb == 0 {
            continue;
        }
        let bound = 200 * la.min(lb) / (la + lb);
        if bound < 50 || best.is_some_and(|(b, _)| bound <= b) {
            continue;
        }
        let score = similarity(source, added);
        if score >= 50 && best.is_none_or(|(b, _)| score > b) {
            best = Some((score, i));
        }
    }
    best.map(|(_, i)| i)
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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The pairwise similarity rename detection used before lines were
    /// counted once per file: the reference the faster one must match.
    fn reference_similarity(a: &[u8], b: &[u8]) -> u64 {
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

    /// Files of a few short lines drawn from a small alphabet, so that they
    /// share lines often and are sometimes empty.
    fn file() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(prop::sample::select(&b"ab\n"[..]), 0..48)
    }

    proptest! {
        #[test]
        fn most_similar_matches_pairwise_comparison(
            added in file(),
            sources in prop::collection::vec(file(), 0..8),
        ) {
            let mut expected: Option<(u64, usize)> = None;
            for (i, source) in sources.iter().enumerate() {
                let score = reference_similarity(source, &added);
                if score >= 50 && expected.is_none_or(|(b, _)| score > b) {
                    expected = Some((score, i));
                }
            }
            let lines: Vec<Lines<'_>> = sources.iter().map(|s| Lines::of(s)).collect();
            let added_lines = Lines::of(&added);
            for (source, l) in sources.iter().zip(&lines) {
                prop_assert_eq!(similarity(l, &added_lines), reference_similarity(source, &added));
            }
            prop_assert_eq!(most_similar(&added_lines, &lines), expected.map(|(_, i)| i));
        }
    }
}
