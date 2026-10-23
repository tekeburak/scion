//! `status`: how the working copy differs from the index.
//!
//! A file whose kind, size, mtime, ctime and inode still match its index
//! entry is taken as unchanged without reading it; any other file is
//! rehashed. A directory is listed, to find files the index does not know,
//! only when its mtime differs from the one the index recorded: adding,
//! removing or renaming an entry changes it. A recorded time is trusted only
//! when it is older than the index file itself, so an edit made in the same
//! clock tick as the checkout is still seen. Directories and files are
//! examined on several threads. There are no ignore files: every file
//! outside `.scn/` that the index does not know is untracked unless
//! `scn add` marked it.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use scion_proto::trace::span;
use scion_types::{BlobId, FileType, RepoPath};

use crate::index::{Index, IndexEntry, mtime_ns};
use crate::workspace::SCN_DIR;

/// Most threads one `status` uses.
const MAX_THREADS: usize = 8;

/// Fewest directories or files a thread is given; below that, spawning it
/// costs more than it saves.
const MIN_PER_THREAD: usize = 128;

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

/// `f` over `items`, split across up to [`MAX_THREADS`] threads, results in
/// order.
fn in_parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(MAX_THREADS));
    let per = items.len().div_ceil(threads).max(MIN_PER_THREAD);
    if items.len() <= per {
        return items.iter().map(f).collect();
    }
    std::thread::scope(|s| {
        let f = &f;
        let handles: Vec<_> = items
            .chunks(per)
            .map(|chunk| s.spawn(move || chunk.iter().map(f).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    })
}

/// Every directory holding an index entry, `""` for the root, parents
/// before children.
fn index_dirs(index: &Index) -> BTreeSet<&str> {
    let mut dirs = BTreeSet::from([""]);
    for path in index.entries.keys() {
        let p = path.as_str();
        dirs.extend(p.match_indices('/').map(|(i, _)| &p[..i]));
    }
    dirs
}

fn dir_path(root: &Path, dir: &str) -> PathBuf {
    if dir.is_empty() {
        root.to_owned()
    } else {
        root.join(dir)
    }
}

fn child(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// Whether an entry of `dir` named `name` is something the index puts
/// there: an index entry, a directory holding some, or the root's `.scn/`.
fn expected(index: &Index, dirs: &BTreeSet<&str>, dir: &str, name: &str, is_dir: bool) -> bool {
    let path = child(dir, name);
    if is_dir {
        (dir.is_empty() && name == SCN_DIR) || dirs.contains(path.as_str())
    } else {
        RepoPath::new(path).is_ok_and(|p| index.entries.contains_key(&p))
    }
}

/// The directories to record in the index: each one holding an index entry
/// whose listing holds nothing else, with its mtime. The mtime is taken
/// before the listing, so a change made during it shows as a newer mtime.
/// A directory that cannot be read is left out and listed by every
/// `status`.
#[must_use]
pub fn clean_dirs(root: &Path, index: &Index) -> BTreeMap<String, i64> {
    let dirs = index_dirs(index);
    let list: Vec<&str> = dirs.iter().copied().collect();
    let clean = in_parallel(&list, |dir| {
        let path = dir_path(root, dir);
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if !meta.is_dir() {
            return None;
        }
        for entry in std::fs::read_dir(&path).ok()? {
            let entry = entry.ok()?;
            let is_dir = entry.file_type().ok()?.is_dir();
            let name = entry.file_name();
            if !expected(index, &dirs, dir, name.to_str()?, is_dir) {
                return None;
            }
        }
        Some(((*dir).to_owned(), mtime_ns(&meta)))
    });
    clean.into_iter().flatten().collect()
}

/// What examining one index directory found.
enum Listed {
    /// Not a directory any more (gone, or a file or symlink in its place):
    /// every index entry beneath it is deleted.
    Gone,
    /// The files under it the index does not know (none when its mtime
    /// matches the recorded one).
    Extra(Vec<PathBuf>),
}

fn list_dir(
    root: &Path,
    index: &Index,
    dirs: &BTreeSet<&str>,
    dir: &str,
) -> Result<Listed, StatusError> {
    let path = dir_path(root, dir);
    let io_err = |e| StatusError::Io(path.clone(), e);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if is_missing(&e) => return Ok(Listed::Gone),
        Err(e) => return Err(io_err(e)),
    };
    if !meta.is_dir() {
        return Ok(Listed::Gone);
    }
    let mtime = mtime_ns(&meta);
    if index.dirs.get(dir) == Some(&mtime) && mtime < index.written_ns {
        return Ok(Listed::Extra(Vec::new()));
    }
    let mut extra = Vec::new();
    for entry in std::fs::read_dir(&path).map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        let is_dir = entry.file_type().map_err(io_err)?.is_dir();
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| expected(index, dirs, dir, name, is_dir))
        {
            continue;
        }
        if is_dir {
            walk(root, &entry.path(), &mut extra)?;
        } else {
            extra.push(entry.path());
        }
    }
    Ok(Listed::Extra(extra))
}

fn is_missing(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// How the file at `path` differs from `entry`, if it does.
fn compare(
    root: &Path,
    index: &Index,
    path: &RepoPath,
    entry: &IndexEntry,
) -> Result<Option<FileStatus>, StatusError> {
    let file = root.join(path.as_str());
    let io_err = |e| StatusError::Io(file.clone(), e);
    let meta = match std::fs::symlink_metadata(&file) {
        Ok(meta) => meta,
        Err(e) if is_missing(&e) => return Ok(Some(FileStatus::Deleted)),
        Err(e) => return Err(io_err(e)),
    };
    if meta.is_dir() {
        return Ok(Some(FileStatus::Deleted));
    }
    if entry.matches(kind_of(&meta), &meta) && entry.mtime_ns < index.written_ns {
        return Ok(None);
    }
    let (kind, data) = read_working_file(&file).map_err(io_err)?;
    Ok((kind != entry.kind || BlobId::hash(&data) != entry.id).then_some(FileStatus::Modified))
}

/// The parent of an index directory, `None` for the root.
fn parent(dir: &str) -> Option<&str> {
    match dir.rfind('/') {
        Some(i) => Some(&dir[..i]),
        None => (!dir.is_empty()).then_some(""),
    }
}

/// Compare the working copy under `root` with `index`; untracked paths in
/// `added` are reported as added. Unchanged files are left out.
pub fn status(
    root: &Path,
    index: &Index,
    added: &BTreeSet<RepoPath>,
) -> Result<BTreeMap<RepoPath, FileStatus>, StatusError> {
    let walked = span("walk");
    let dirs = index_dirs(index);
    let list: Vec<&str> = dirs.iter().copied().collect();
    let listed = in_parallel(&list, |dir| list_dir(root, index, &dirs, dir));
    // A directory under one that is gone was reached through whatever
    // replaced it (a symlink, say), so what it shows is not the working
    // copy's.
    let mut gone: BTreeSet<&str> = BTreeSet::new();
    let mut extra = Vec::new();
    for (dir, listed) in list.iter().zip(listed) {
        if parent(dir).is_some_and(|p| gone.contains(p)) {
            gone.insert(dir);
            continue;
        }
        match listed? {
            Listed::Gone => {
                gone.insert(dir);
            }
            Listed::Extra(files) => extra.extend(files),
        }
    }
    drop(walked);
    let _compare = span("compare");
    let entries: Vec<(&RepoPath, &IndexEntry)> = index.entries.iter().collect();
    let compared = in_parallel(&entries, |(path, entry)| {
        let under_gone = path
            .as_str()
            .rfind('/')
            .is_some_and(|i| gone.contains(&path.as_str()[..i]));
        if under_gone {
            Ok(Some(FileStatus::Deleted))
        } else {
            compare(root, index, path, entry)
        }
    });
    let mut out = BTreeMap::new();
    for ((path, _), st) in entries.iter().zip(compared) {
        if let Some(st) = st? {
            out.insert((*path).clone(), st);
        }
    }
    for file in extra {
        let path = file
            .strip_prefix(root)
            .ok()
            .and_then(Path::to_str)
            .and_then(|s| RepoPath::new(s).ok())
            .ok_or_else(|| StatusError::BadPath(file.clone()))?;
        let st = if added.contains(&path) {
            FileStatus::Added
        } else {
            FileStatus::Untracked
        };
        out.insert(path, st);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `files` under `root` and index them as checked out, every
    /// recorded time trusted.
    fn checked_out(root: &Path, files: &[(&str, &[u8])]) -> Index {
        let mut index = Index {
            written_ns: i64::MAX,
            ..Index::default()
        };
        for (path, data) in files {
            let dest = root.join(path);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(&dest, data).unwrap();
            let meta = std::fs::symlink_metadata(&dest).unwrap();
            index.entries.insert(
                RepoPath::new(*path).unwrap(),
                IndexEntry::new(BlobId::hash(data), FileType::Regular, &meta),
            );
        }
        index.dirs = clean_dirs(root, &index);
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
                ctime_ns: 0,
                ino: 0,
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

    #[test]
    fn unchanged_directories_are_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let index = checked_out(root, &[("d/a", b"a\n"), ("e/b", b"b\n")]);
        assert_eq!(index.dirs.len(), 3);
        // A file created behind a directory mtime set back to the recorded
        // one is not seen: the listing was skipped.
        std::fs::write(root.join("d/new"), b"n\n").unwrap();
        std::fs::write(root.join("e/new"), b"n\n").unwrap();
        std::fs::File::open(root.join("d"))
            .unwrap()
            .set_modified(
                std::time::UNIX_EPOCH
                    + std::time::Duration::from_nanos(index.dirs["d"].try_into().unwrap()),
            )
            .unwrap();
        let got: Vec<String> = status(root, &index, &BTreeSet::new())
            .unwrap()
            .into_keys()
            .map(|p| p.as_str().to_owned())
            .collect();
        assert_eq!(got, ["e/new"]);
    }

    #[test]
    fn untracked_files_keep_a_directory_unrecorded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(SCN_DIR)).unwrap();
        std::fs::create_dir_all(root.join("d/sub")).unwrap();
        std::fs::write(root.join("d/sub/x"), b"x").unwrap();
        let index = checked_out(root, &[("d/a", b"a\n"), ("e/b", b"b\n")]);
        assert_eq!(
            index.dirs.keys().map(String::as_str).collect::<Vec<_>>(),
            ["", "e"]
        );
    }

    /// One edit to the working copy, on the file or directory `n` picks.
    #[derive(Debug, Clone)]
    enum Edit {
        Rewrite(usize, Vec<u8>),
        Touch(usize),
        Delete(usize),
        Chmod(usize),
        Symlink(usize),
        FileToDir(usize),
        DirToFile(usize),
        DirToSymlink(usize),
        Create(usize, String),
    }

    fn edit() -> impl proptest::strategy::Strategy<Value = Edit> {
        use proptest::prelude::*;
        prop_oneof![
            (any::<usize>(), proptest::collection::vec(any::<u8>(), 0..4))
                .prop_map(|(n, d)| Edit::Rewrite(n, d)),
            any::<usize>().prop_map(Edit::Touch),
            any::<usize>().prop_map(Edit::Delete),
            any::<usize>().prop_map(Edit::Chmod),
            any::<usize>().prop_map(Edit::Symlink),
            any::<usize>().prop_map(Edit::FileToDir),
            any::<usize>().prop_map(Edit::DirToFile),
            any::<usize>().prop_map(Edit::DirToSymlink),
            (any::<usize>(), "[a-c]{1,2}").prop_map(|(n, name)| Edit::Create(n, name)),
        ]
    }

    /// Every file and every directory under `root`, outside `.scn/`.
    fn everything(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut files = Vec::new();
        walk(root, root, &mut files).unwrap();
        let mut dirs = vec![root.to_owned()];
        let mut i = 0;
        while i < dirs.len() {
            for entry in std::fs::read_dir(&dirs[i]).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() && entry.file_name() != SCN_DIR {
                    dirs.push(entry.path());
                }
            }
            i += 1;
        }
        files.sort();
        dirs.sort();
        (files, dirs)
    }

    fn apply(root: &Path, edit: &Edit) {
        let (files, dirs) = everything(root);
        let pick = |v: &[PathBuf], n: usize| (!v.is_empty()).then(|| v[n % v.len()].clone());
        match edit {
            Edit::Rewrite(n, data) => {
                if let Some(f) = pick(&files, *n) {
                    let _ = std::fs::remove_file(&f);
                    std::fs::write(f, data).unwrap();
                }
            }
            Edit::Touch(n) => {
                if let Some(f) = pick(&files, *n) {
                    if let Ok(data) = std::fs::read(&f) {
                        std::fs::write(f, data).unwrap();
                    }
                }
            }
            Edit::Delete(n) => {
                if let Some(f) = pick(&files, *n) {
                    std::fs::remove_file(f).unwrap();
                }
            }
            Edit::Chmod(n) => {
                if let Some(f) = pick(&files, *n) {
                    if std::fs::symlink_metadata(&f).unwrap().is_file() {
                        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755))
                            .unwrap();
                    }
                }
            }
            Edit::Symlink(n) => {
                if let Some(f) = pick(&files, *n) {
                    std::fs::remove_file(&f).unwrap();
                    std::os::unix::fs::symlink("t", f).unwrap();
                }
            }
            Edit::FileToDir(n) => {
                if let Some(f) = pick(&files, *n) {
                    std::fs::remove_file(&f).unwrap();
                    std::fs::create_dir(&f).unwrap();
                    std::fs::write(f.join("inner"), b"i").unwrap();
                }
            }
            Edit::DirToFile(n) => {
                if let Some(d) = pick(&dirs[1..], *n) {
                    std::fs::remove_dir_all(&d).unwrap();
                    std::fs::write(d, b"was a directory").unwrap();
                }
            }
            Edit::DirToSymlink(n) => {
                if let Some(d) = pick(&dirs[1..], *n) {
                    let aside = root.join(SCN_DIR).join("aside");
                    let _ = std::fs::remove_dir_all(&aside);
                    std::fs::rename(&d, &aside).unwrap();
                    std::os::unix::fs::symlink(&aside, d).unwrap();
                }
            }
            Edit::Create(n, name) => {
                let d = pick(&dirs, *n).unwrap();
                let path = d.join(name);
                if std::fs::symlink_metadata(&path).is_err() {
                    std::fs::write(path, b"new").unwrap();
                }
            }
        }
    }

    /// `status` the slow way: list everything, rehash every file.
    fn rehashed(
        root: &Path,
        index: &Index,
        added: &BTreeSet<RepoPath>,
    ) -> BTreeMap<RepoPath, FileStatus> {
        let mut out = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for file in everything(root).0 {
            let path = RepoPath::new(file.strip_prefix(root).unwrap().to_str().unwrap()).unwrap();
            match index.entries.get(&path) {
                None if added.contains(&path) => {
                    out.insert(path, FileStatus::Added);
                }
                None => {
                    out.insert(path, FileStatus::Untracked);
                }
                Some(entry) => {
                    let (kind, data) = read_working_file(&file).unwrap();
                    if kind != entry.kind || BlobId::hash(&data) != entry.id {
                        out.insert(path.clone(), FileStatus::Modified);
                    }
                    seen.insert(path);
                }
            }
        }
        for path in index.entries.keys() {
            if !seen.contains(path) {
                out.insert(path.clone(), FileStatus::Deleted);
            }
        }
        out
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        #[test]
        fn matches_a_full_rehash(
            files in proptest::collection::btree_map(
                "[a-c]{1,2}(/[a-c]{1,2}){0,2}",
                proptest::collection::vec(proptest::prelude::any::<u8>(), 0..4),
                1..24,
            ),
            edits in proptest::collection::vec(edit(), 0..8),
            add in proptest::prelude::any::<bool>(),
        ) {
            use crate::workspace::{Config, Workspace};
            // Keep only paths that are not also a directory of another.
            let files: Vec<(&String, &Vec<u8>)> = files
                .iter()
                .filter(|(p, _)| !files.keys().any(|q| q.starts_with(&format!("{p}/"))))
                .collect();
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let ws = Workspace::init(
                root,
                &Config { server: "http://localhost".into(), repo: "r".into() },
            )
            .unwrap();
            let mut index = Index::default();
            for (path, data) in &files {
                let dest = root.join(path);
                std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
                std::fs::write(&dest, data).unwrap();
                let meta = std::fs::symlink_metadata(&dest).unwrap();
                index.entries.insert(
                    RepoPath::new(path.as_str()).unwrap(),
                    IndexEntry::new(BlobId::hash(data), FileType::Regular, &meta),
                );
            }
            ws.write_index(&mut index).unwrap();
            let index = ws.read_index().unwrap();
            for e in &edits {
                apply(root, e);
            }
            let added: BTreeSet<RepoPath> = if add {
                rehashed(root, &index, &BTreeSet::new())
                    .into_iter()
                    .filter(|(_, st)| *st == FileStatus::Untracked)
                    .map(|(p, _)| p)
                    .take(1)
                    .collect()
            } else {
                BTreeSet::new()
            };
            proptest::prop_assert_eq!(
                status(root, &index, &added).unwrap(),
                rehashed(root, &index, &added)
            );
        }
    }
}
