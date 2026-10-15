//! The index: what the working copy has checked out.
//!
//! Each entry records the blob a file was written from plus the size and
//! mtime it had afterwards, so `status` can skip rehashing unchanged files.
//! The index never leaves the machine and is never hashed; it reuses the
//! canonical primitives only for a strict, compact on-disk form:
//!
//! ```text
//! magic    4 bytes "SCIX"
//! version  u16     1
//! entries  seq(str path, id blob, u8 kind, u64 size, i64 mtime_ns)
//!          strictly increasing by path
//! ```

use std::collections::BTreeMap;

use scion_types::canonical::{CanonicalError, Reader, Writer};
use scion_types::{BlobId, FileType, RepoPath};

const MAGIC: &[u8; 4] = b"SCIX";
const VERSION: u16 = 1;

/// One checked-out file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    pub id: BlobId,
    pub kind: FileType,
    pub size: u64,
    /// Modification time after checkout, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
}

/// Every checked-out file, by path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    pub entries: BTreeMap<RepoPath, IndexEntry>,
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
        if version != VERSION {
            return Err(CanonicalError::UnsupportedVersion(version));
        }
        let mut entries = BTreeMap::new();
        let mut last: Option<RepoPath> = None;
        for _ in 0..r.seq_len()? {
            let path = RepoPath::new(r.str()?)?;
            if last.as_ref().is_some_and(|l| *l >= path) {
                return Err(CanonicalError::UnsortedKeys);
            }
            let entry = IndexEntry {
                id: BlobId::from_bytes(r.id()?),
                kind: kind_from_tag(r.u8()?)?,
                size: r.u64()?,
                mtime_ns: r.i64()?,
            };
            last = Some(path.clone());
            entries.insert(path, entry);
        }
        r.finish()?;
        Ok(Self { entries })
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
                },
            );
        }
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
