//! The index: what the working copy has checked out.
//!
//! Each entry records the blob a file was written from plus the size, mtime,
//! ctime and inode it had afterwards, so `status` can skip rehashing
//! unchanged files. Each directory whose listing held only what the index
//! puts there when the index was written is recorded with its mtime, so
//! `status` can skip listing it while that mtime holds. The index never
//! leaves the machine and is never hashed; it reuses the canonical
//! primitives only for a strict, compact on-disk form:
//!
//! ```text
//! magic    4 bytes "SCIX"
//! version  u16     2
//! entries  seq(str path, id blob, u8 kind, u64 size, i64 mtime_ns,
//!              i64 ctime_ns, u64 ino)
//!          strictly increasing by path
//! dirs     seq(str path, i64 mtime_ns)
//!          strictly increasing by path, "" for the root
//! ```
//!
//! Version 1 had neither the ctime and inode nor the directories; it is
//! still read, and every file it lists is rehashed until the index is next
//! written.

use std::collections::BTreeMap;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;

use scion_types::canonical::{CanonicalError, Reader, Writer};
use scion_types::{BlobId, FileType, RepoPath};

const MAGIC: &[u8; 4] = b"SCIX";
const VERSION: u16 = 2;

/// One checked-out file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    pub id: BlobId,
    pub kind: FileType,
    pub size: u64,
    /// Modification time after checkout, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Inode change time after checkout, nanoseconds since the Unix epoch.
    pub ctime_ns: i64,
    pub ino: u64,
}

impl IndexEntry {
    /// The entry for blob `id` of `kind`, with the size, times and inode
    /// `meta` shows.
    #[must_use]
    pub fn new(id: BlobId, kind: FileType, meta: &Metadata) -> Self {
        Self {
            id,
            kind,
            size: meta.len(),
            mtime_ns: mtime_ns(meta),
            ctime_ns: ctime_ns(meta),
            ino: meta.ino(),
        }
    }

    /// Whether a file of `kind` with metadata `meta` still has the size,
    /// times and inode this entry recorded.
    #[must_use]
    pub fn matches(&self, kind: FileType, meta: &Metadata) -> bool {
        kind == self.kind
            && meta.len() == self.size
            && mtime_ns(meta) == self.mtime_ns
            && ctime_ns(meta) == self.ctime_ns
            && meta.ino() == self.ino
    }
}

/// `meta`'s modification time, nanoseconds since the Unix epoch.
#[must_use]
pub fn mtime_ns(meta: &Metadata) -> i64 {
    meta.mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(meta.mtime_nsec())
}

fn ctime_ns(meta: &Metadata) -> i64 {
    meta.ctime()
        .saturating_mul(1_000_000_000)
        .saturating_add(meta.ctime_nsec())
}

/// Every checked-out file, by path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    pub entries: BTreeMap<RepoPath, IndexEntry>,
    /// Directories that held only index entries and their directories when
    /// the index was written, with their mtime then; `""` is the root.
    /// [`crate::workspace::Workspace::write_index`] fills it in.
    pub dirs: BTreeMap<String, i64>,
    /// The index file's own mtime when it was read, 0 for an index not read
    /// from disk. A recorded mtime at or after it may have been taken in
    /// the same clock tick as a later edit, so it proves nothing.
    pub written_ns: i64,
}

fn kind_from_tag(tag: u8) -> Result<FileType, CanonicalError> {
    match tag {
        0 => Ok(FileType::Regular),
        1 => Ok(FileType::Executable),
        2 => Ok(FileType::Symlink),
        tag => Err(CanonicalError::BadTag {
            what: "file type",
            tag,
        }),
    }
}

impl Index {
    /// The on-disk form.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        for b in MAGIC {
            w.u8(*b);
        }
        w.u16(VERSION);
        w.seq(self.entries.iter(), |w, (path, e)| {
            w.str(path.as_str());
            w.id(e.id.as_bytes());
            w.u8(e.kind.tag());
            w.u64(e.size);
            w.i64(e.mtime_ns);
            w.i64(e.ctime_ns);
            w.u64(e.ino);
        });
        w.seq(self.dirs.iter(), |w, (path, mtime_ns)| {
            w.str(path);
            w.i64(*mtime_ns);
        });
        w.into_vec()
    }

    /// Parse the on-disk form, rejecting anything [`Index::encode`] would not
    /// produce.
    pub fn decode(bytes: &[u8]) -> Result<Self, CanonicalError> {
        let mut r = Reader::new(bytes);
        let mut magic = [0u8; 4];
        for b in &mut magic {
            *b = r.u8()?;
        }
        if &magic != MAGIC {
            return Err(CanonicalError::BadMagic {
                expected: *MAGIC,
                got: magic,
            });
        }
        let version = r.u16()?;
        if version != VERSION && version != 1 {
            return Err(CanonicalError::UnsupportedVersion(version));
        }
        let mut entries = BTreeMap::new();
        let mut last: Option<RepoPath> = None;
        for _ in 0..r.seq_len()? {
            let path = RepoPath::new(r.str()?)?;
            if last.as_ref().is_some_and(|l| *l >= path) {
                return Err(CanonicalError::UnsortedKeys);
            }
            let id = BlobId::from_bytes(r.id()?);
            let kind = kind_from_tag(r.u8()?)?;
            let size = r.u64()?;
            let mtime_ns = r.i64()?;
            let (ctime_ns, ino) = if version == 1 {
                (0, 0)
            } else {
                (r.i64()?, r.u64()?)
            };
            last = Some(path.clone());
            entries.insert(
                path,
                IndexEntry {
                    id,
                    kind,
                    size,
                    mtime_ns,
                    ctime_ns,
                    ino,
                },
            );
        }
        let mut dirs = BTreeMap::new();
        if version != 1 {
            let mut last: Option<&str> = None;
            for _ in 0..r.seq_len()? {
                let path = r.str()?;
                if last.is_some_and(|l| l >= path) {
                    return Err(CanonicalError::UnsortedKeys);
                }
                last = Some(path);
                dirs.insert(path.to_owned(), r.i64()?);
            }
        }
        r.finish()?;
        Ok(Self {
            entries,
            dirs,
            written_ns: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut index = Index::default();
        for (i, (path, kind)) in (0u8..).zip([
            ("a.txt", FileType::Regular),
            ("bin/run", FileType::Executable),
            ("link", FileType::Symlink),
        ]) {
            index.entries.insert(
                RepoPath::new(path).unwrap(),
                IndexEntry {
                    id: BlobId::hash(path.as_bytes()),
                    kind,
                    size: u64::from(i),
                    mtime_ns: i64::from(i) - 1,
                    ctime_ns: i64::from(i) + 7,
                    ino: u64::from(i) * 3,
                },
            );
        }
        index.dirs.insert(String::new(), 5);
        index.dirs.insert("bin".to_owned(), -2);
        index
    }

    #[test]
    fn round_trips() {
        let index = sample();
        assert_eq!(Index::decode(&index.encode()).unwrap(), index);
        assert_eq!(
            Index::decode(&Index::default().encode()).unwrap(),
            Index::default()
        );
    }

    #[test]
    fn reads_version_1() {
        let mut w = Writer::new();
        for b in MAGIC {
            w.u8(*b);
        }
        w.u16(1);
        w.seq(std::iter::once(()), |w, ()| {
            w.str("a.txt");
            w.id(BlobId::hash(b"a").as_bytes());
            w.u8(FileType::Regular.tag());
            w.u64(1);
            w.i64(9);
        });
        let index = Index::decode(&w.into_vec()).unwrap();
        let entry = index.entries[&RepoPath::new("a.txt").unwrap()];
        assert_eq!((entry.mtime_ns, entry.ctime_ns, entry.ino), (9, 0, 0));
        assert!(index.dirs.is_empty());
    }

    #[test]
    fn rejects_garbage() {
        let bytes = sample().encode();
        assert!(Index::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(Index::decode(&trailing).is_err());
        let mut magic = bytes.clone();
        magic[0] = b'X';
        assert!(Index::decode(&magic).is_err());
        assert!(Index::decode(b"").is_err());
    }
}
