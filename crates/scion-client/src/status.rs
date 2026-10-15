//! `status`: how the working copy differs from the index.
//!
//! A file whose kind, size and mtime still match its index entry is taken as
//! unchanged without reading it; any other file is rehashed. There are no
//! ignore files: every file outside `.scn/` that the index does not know is
//! untracked unless `scn add` marked it.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use scion_types::{BlobId, FileType, RepoPath};

use crate::checkout::mtime_ns;
use crate::index::Index;
use crate::workspace::SCN_DIR;

/// How one path differs from the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// In the index, with different content or kind on disk.
    Modified,
    /// In the index, gone from disk.
    Deleted,
    /// On disk, not in the index, marked by `scn add`.
    Added,
    /// On disk, not in the index, not added.
    Untracked,
}

impl FileStatus {
    /// The one-letter code `scn status` prints.
    #[must_use]
    pub const fn code(self) -> char {
        match self {
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Added => 'A',
            Self::Untracked => '?',
        }
    }
}

/// Failure reading the working copy.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("{0}: {1}")]
    Io(PathBuf, #[source] io::Error),
    #[error("{0}: not a valid repository path")]
    BadPath(PathBuf),
}

fn kind_of(meta: &std::fs::Metadata) -> FileType {
    if meta.file_type().is_symlink() {
        FileType::Symlink
    } else if meta.permissions().mode() & 0o111 != 0 {
        FileType::Executable
    } else {
        FileType::Regular
    }
}

/// The kind and blob bytes of the file at `path`: its contents, or the
/// target of a symlink.
pub fn read_working_file(path: &Path) -> io::Result<(FileType, Vec<u8>)> {
    let meta = std::fs::symlink_metadata(path)?;
    let kind = kind_of(&meta);
    let data = match kind {
        FileType::Symlink => std::fs::read_link(path)?
            .into_os_string()
            .into_encoded_bytes(),
        FileType::Regular | FileType::Executable => std::fs::read(path)?,
    };
    Ok((kind, data))
}

/// Every file under `dir` (not following symlinks), skipping the top-level
/// `.scn/`.
fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), StatusError> {
    let io_err = |e| StatusError::Io(dir.to_owned(), e);
    for entry in std::fs::read_dir(dir).map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        let path = entry.path();
        if dir == root && entry.file_name() == SCN_DIR {
            continue;
        }
        if entry.file_type().map_err(io_err)?.is_dir() {
            walk(root, &path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Compare the working copy under `root` with `index`; untracked paths in
/// `added` are reported as added. Unchanged files are left out.
pub fn status(
    root: &Path,
    index: &Index,
    added: &BTreeSet<RepoPath>,
) -> Result<BTreeMap<RepoPath, FileStatus>, StatusError> {
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    let mut out = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for file in files {
        let path = file
            .strip_prefix(root)
            .ok()
            .and_then(Path::to_str)
            .and_then(|s| RepoPath::new(s).ok())
            .ok_or_else(|| StatusError::BadPath(file.clone()))?;
        let Some(entry) = index.entries.get(&path) else {
            let st = if added.contains(&path) {
                FileStatus::Added
            } else {
                FileStatus::Untracked
            };
            out.insert(path, st);
            continue;
        };
        seen.insert(path.clone());
        let io_err = |e| StatusError::Io(file.clone(), e);
        let meta = std::fs::symlink_metadata(&file).map_err(io_err)?;
        if kind_of(&meta) == entry.kind
            && meta.len() == entry.size
            && mtime_ns(&meta) == entry.mtime_ns
        {
            continue;
        }
        let (kind, data) = read_working_file(&file).map_err(io_err)?;
        if kind != entry.kind || BlobId::hash(&data) != entry.id {
            out.insert(path, FileStatus::Modified);
        }
    }
    for path in index.entries.keys() {
        if !seen.contains(path) {
            out.insert(path.clone(), FileStatus::Deleted);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexEntry;

    /// Write `files` under `root` and index them as checked out.
    fn checked_out(root: &Path, files: &[(&str, &[u8])]) -> Index {
        let mut index = Index::default();
        for (path, data) in files {
            let dest = root.join(path);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(&dest, data).unwrap();
            let meta = std::fs::symlink_metadata(&dest).unwrap();
            index.entries.insert(
                RepoPath::new(*path).unwrap(),
                IndexEntry {
                    id: BlobId::hash(data),
                    kind: FileType::Regular,
                    size: data.len() as u64,
                    mtime_ns: mtime_ns(&meta),
                },
            );
        }
        index
    }

    #[test]
    fn reports_modified_deleted_and_untracked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(SCN_DIR)).unwrap();
        std::fs::write(root.join(SCN_DIR).join("index"), b"ignored").unwrap();
        let index = checked_out(
            root,
            &[
                ("a.txt", b"a\n"),
                ("d/b.txt", b"b\n"),
                ("d/c.txt", b"c\n"),
                ("exe", b"x\n"),
                ("same", b"s\n"),
            ],
        );
        assert!(status(root, &index, &BTreeSet::new()).unwrap().is_empty());

        std::fs::write(root.join("a.txt"), b"changed\n").unwrap();
        std::fs::remove_file(root.join("d/b.txt")).unwrap();
        std::fs::write(root.join("d/new.txt"), b"n\n").unwrap();
        std::fs::set_permissions(root.join("exe"), std::fs::Permissions::from_mode(0o755)).unwrap();
        // Same bytes, new mtime: rehashed and found unchanged.
        std::fs::write(root.join("same"), b"s\n").unwrap();

        let added = BTreeSet::from([RepoPath::new("d/new.txt").unwrap()]);
        std::fs::write(root.join("other.txt"), b"o\n").unwrap();
        let got: Vec<(String, char)> = status(root, &index, &added)
            .unwrap()
            .into_iter()
            .map(|(p, s)| (p.as_str().to_owned(), s.code()))
            .collect();
        assert_eq!(
            got,
            [
                ("a.txt".to_owned(), 'M'),
                ("d/b.txt".to_owned(), 'D'),
                ("d/new.txt".to_owned(), 'A'),
                ("exe".to_owned(), 'M'),
                ("other.txt".to_owned(), '?'),
            ]
        );
    }

    #[test]
    fn symlinks_are_compared_by_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink("target", root.join("link")).unwrap();
        let mut index = Index::default();
        index.entries.insert(
            RepoPath::new("link").unwrap(),
            IndexEntry {
                id: BlobId::hash(b"target"),
                kind: FileType::Symlink,
                size: 6,
                mtime_ns: 0,
            },
        );
        assert!(status(root, &index, &BTreeSet::new()).unwrap().is_empty());
        std::fs::remove_file(root.join("link")).unwrap();
        std::os::unix::fs::symlink("elsewhere", root.join("link")).unwrap();
        assert_eq!(
            status(root, &index, &BTreeSet::new()).unwrap()[&RepoPath::new("link").unwrap()],
            FileStatus::Modified
        );
    }
}
