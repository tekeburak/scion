//! Canonical byte encoding for content-addressed objects.
//!
//! This is the *only* encoding that feeds a hash. It is specified byte-for-byte
//! in `docs/DATA_MODEL.md` and guarded by golden-hash tests; any change here is
//! a format version bump.
//!
//! Primitives (all integers big-endian):
//!
//! | name   | bytes                                   |
//! |--------|-----------------------------------------|
//! | u8     | 1                                       |
//! | u16    | 2                                       |
//! | i32    | 4, two's complement                     |
//! | u64    | 8                                       |
//! | i64    | 8, two's complement                     |
//! | id     | 32 raw digest bytes                     |
//! | bytes  | u64 length, then that many raw bytes    |
//! | str    | `bytes` of the UTF-8 encoding           |
//! | opt(T) | u8 `0x00` (absent) or `0x01` then T     |
//! | seq(T) | u64 count, then count × T               |
//!
//! Every object starts with a 4-byte magic and a u16 format version.
//! Maps are encoded as `seq` of `(key, value)` pairs in strictly increasing
//! byte order of the key; decoders reject unsorted or duplicate keys. Decoders
//! also reject trailing bytes, so there is exactly one valid encoding per value.

use std::collections::BTreeMap;

use crate::{
    BlobId, Changeset, ChangesetId, FileChange, FileType, Identity, PathComponent, RepoPath,
    Timestamp, Tree, TreeEntry, TreeId,
};

/// Magic prefix of a canonical changeset.
pub const CHANGESET_MAGIC: &[u8; 4] = b"SCCS";
/// Magic prefix of a canonical tree.
pub const TREE_MAGIC: &[u8; 4] = b"SCTR";
/// Current format version for both object kinds.
pub const FORMAT_VERSION: u16 = 1;

/// Error produced when decoding a canonical byte string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CanonicalError {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("bad magic: expected {expected:?}, got {got:?}")]
    BadMagic { expected: [u8; 4], got: [u8; 4] },
    #[error("unsupported format version {0}")]
    UnsupportedVersion(u16),
    #[error("bad {what} tag {tag:#04x}")]
    BadTag { what: &'static str, tag: u8 },
    #[error("invalid UTF-8 in string")]
    InvalidUtf8,
    #[error("invalid path: {0}")]
    InvalidPath(#[from] crate::PathError),
    #[error("map keys are not strictly increasing")]
    UnsortedKeys,
    #[error("trailing bytes after object")]
    TrailingBytes,
    #[error("length does not fit in memory")]
    LengthOverflow,
}

/// A growable canonical byte writer.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

fn len_u64(n: usize) -> u64 {
    u64::try_from(n).expect("usize fits in u64")
}

impl Writer {
    /// A fresh writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the encoded bytes.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn id(&mut self, id: &[u8; 32]) {
        self.buf.extend_from_slice(id);
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.u64(len_u64(b.len()));
        self.buf.extend_from_slice(b);
    }
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    pub fn opt<T>(&mut self, v: Option<&T>, f: impl FnOnce(&mut Self, &T)) {
        match v {
            None => self.u8(0),
            Some(v) => {
                self.u8(1);
                f(self, v);
            }
        }
    }
    pub fn seq<T>(
        &mut self,
        items: impl ExactSizeIterator<Item = T>,
        mut f: impl FnMut(&mut Self, T),
    ) {
        self.u64(len_u64(items.len()));
        for item in items {
            f(self, item);
        }
    }
}

/// A strict canonical byte reader.
#[derive(Debug)]
pub struct Reader<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Start reading `input`.
    #[must_use]
    pub fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CanonicalError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(CanonicalError::LengthOverflow)?;
        if end > self.input.len() {
            return Err(CanonicalError::UnexpectedEof);
        }
        let s = &self.input[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    /// Fail unless every byte has been consumed.
    pub fn finish(self) -> Result<(), CanonicalError> {
        if self.pos == self.input.len() {
            Ok(())
        } else {
            Err(CanonicalError::TrailingBytes)
        }
    }

    pub fn u8(&mut self) -> Result<u8, CanonicalError> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, CanonicalError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }
    pub fn i32(&mut self) -> Result<i32, CanonicalError> {
        Ok(i32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    pub fn u64(&mut self) -> Result<u64, CanonicalError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    pub fn i64(&mut self) -> Result<i64, CanonicalError> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    pub fn id(&mut self) -> Result<[u8; 32], CanonicalError> {
        Ok(self.take(32)?.try_into().expect("32 bytes"))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], CanonicalError> {
        let n = self.u64()?;
        let n = usize::try_from(n).map_err(|_| CanonicalError::LengthOverflow)?;
        self.take(n)
    }
    pub fn str(&mut self) -> Result<&'a str, CanonicalError> {
        std::str::from_utf8(self.bytes()?).map_err(|_| CanonicalError::InvalidUtf8)
    }
    pub fn opt<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, CanonicalError>,
    ) -> Result<Option<T>, CanonicalError> {
        match self.u8()? {
            0 => Ok(None),
            1 => f(self).map(Some),
            tag => Err(CanonicalError::BadTag {
                what: "option",
                tag,
            }),
        }
    }
    /// Read a `seq` count. The caller loops; we do not pre-allocate from an
    /// untrusted count.
    pub fn seq_len(&mut self) -> Result<u64, CanonicalError> {
        self.u64()
    }
    fn magic(&mut self, expected: [u8; 4]) -> Result<(), CanonicalError> {
        let got: [u8; 4] = self.take(4)?.try_into().expect("4 bytes");
        if got == expected {
            Ok(())
        } else {
            Err(CanonicalError::BadMagic { expected, got })
        }
    }
    fn version(&mut self) -> Result<(), CanonicalError> {
        match self.u16()? {
            FORMAT_VERSION => Ok(()),
            v => Err(CanonicalError::UnsupportedVersion(v)),
        }
    }
}

/// Types with a canonical encoding.
pub trait Encode {
    /// Append the canonical encoding to `w`.
    fn encode(&self, w: &mut Writer);

    /// The canonical encoding as a fresh vector.
    fn encode_to_vec(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.encode(&mut w);
        w.into_vec()
    }
}

/// Types that can be strictly decoded from their canonical encoding.
pub trait Decode: Sized {
    /// Decode from `r`, leaving it positioned after the object.
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, CanonicalError>;

    /// Decode exactly one object from `bytes`, rejecting trailing data.
    fn decode(bytes: &[u8]) -> Result<Self, CanonicalError> {
        let mut r = Reader::new(bytes);
        let v = Self::decode_from(&mut r)?;
        r.finish()?;
        Ok(v)
    }
}

// ---- helpers ---------------------------------------------------------------

fn encode_identity(w: &mut Writer, id: &Identity) {
    w.str(&id.name);
    w.str(&id.email);
}
fn decode_identity(r: &mut Reader<'_>) -> Result<Identity, CanonicalError> {
    Ok(Identity {
        name: r.str()?.to_owned(),
        email: r.str()?.to_owned(),
    })
}
fn encode_timestamp(w: &mut Writer, t: &Timestamp) {
    w.i64(t.unix_secs);
    w.i32(t.tz_offset_minutes);
}
fn decode_timestamp(r: &mut Reader<'_>) -> Result<Timestamp, CanonicalError> {
    Ok(Timestamp {
        unix_secs: r.i64()?,
        tz_offset_minutes: r.i32()?,
    })
}

/// Insert into a map while enforcing strictly increasing key order: the
/// map's last key is the previous one.
fn insert_sorted<K: Ord, V>(
    map: &mut BTreeMap<K, V>,
    key: K,
    value: V,
) -> Result<(), CanonicalError> {
    if map.last_key_value().is_some_and(|(prev, _)| *prev >= key) {
        return Err(CanonicalError::UnsortedKeys);
    }
    map.insert(key, value);
    Ok(())
}

// ---- Changeset -------------------------------------------------------------

const FILE_CHANGE_DELETED: u8 = 0;
const FILE_CHANGE_TRACKED: u8 = 1;

impl Encode for Changeset {
    fn encode(&self, w: &mut Writer) {
        w.buf.extend_from_slice(CHANGESET_MAGIC);
        w.u16(FORMAT_VERSION);
        w.str(&self.repo);
        w.seq(self.parents.iter(), |w, p| w.id(p.as_bytes()));
        encode_identity(w, &self.author);
        encode_timestamp(w, &self.author_date);
        w.opt(self.committer.as_ref(), encode_identity);
        w.opt(self.committer_date.as_ref(), encode_timestamp);
        w.str(&self.message);
        w.seq(self.file_changes.iter(), |w, (path, change)| {
            w.str(path.as_str());
            match change {
                FileChange::Deleted => w.u8(FILE_CHANGE_DELETED),
                FileChange::Tracked {
                    content,
                    kind,
                    size,
                    copy_from,
                } => {
                    w.u8(FILE_CHANGE_TRACKED);
                    w.id(content.as_bytes());
                    w.u8(kind.tag());
                    w.u64(*size);
                    w.opt(copy_from.as_ref(), |w, (p, cs)| {
                        w.str(p.as_str());
                        w.id(cs.as_bytes());
                    });
                }
            }
        });
        w.seq(self.extra.iter(), |w, (k, v)| {
            w.str(k);
            w.bytes(v);
        });
    }
}

impl Decode for Changeset {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, CanonicalError> {
        r.magic(*CHANGESET_MAGIC)?;
        r.version()?;
        let repo = r.str()?.to_owned();

        let n = r.seq_len()?;
        let mut parents = Vec::new();
        for _ in 0..n {
            parents.push(ChangesetId::from_bytes(r.id()?));
        }

        let author = decode_identity(r)?;
        let author_date = decode_timestamp(r)?;
        let committer = r.opt(decode_identity)?;
        let committer_date = r.opt(decode_timestamp)?;
        let message = r.str()?.to_owned();

        let n = r.seq_len()?;
        let mut file_changes = BTreeMap::new();
        for _ in 0..n {
            let path = RepoPath::new(r.str()?)?;
            let change = match r.u8()? {
                FILE_CHANGE_DELETED => FileChange::Deleted,
                FILE_CHANGE_TRACKED => {
                    let content = BlobId::from_bytes(r.id()?);
                    let kind = FileType::from_tag(r.u8()?)?;
                    let size = r.u64()?;
                    let copy_from = r.opt(|r| {
                        let p = RepoPath::new(r.str()?)?;
                        let cs = ChangesetId::from_bytes(r.id()?);
                        Ok((p, cs))
                    })?;
                    FileChange::Tracked {
                        content,
                        kind,
                        size,
                        copy_from,
                    }
                }
                tag => {
                    return Err(CanonicalError::BadTag {
                        what: "file change",
                        tag,
                    });
                }
            };
            insert_sorted(&mut file_changes, path, change)?;
        }

        let n = r.seq_len()?;
        let mut extra = BTreeMap::new();
        for _ in 0..n {
            let k = r.str()?.to_owned();
            let v = r.bytes()?.to_vec();
            insert_sorted(&mut extra, k, v)?;
        }

        Ok(Changeset {
            repo,
            parents,
            author,
            author_date,
            committer,
            committer_date,
            message,
            file_changes,
            extra,
        })
    }
}

// ---- Tree ------------------------------------------------------------------

const TREE_ENTRY_TREE: u8 = 0;
const TREE_ENTRY_BLOB: u8 = 1;

impl Encode for Tree {
    fn encode(&self, w: &mut Writer) {
        w.buf.extend_from_slice(TREE_MAGIC);
        w.u16(FORMAT_VERSION);
        w.seq(self.entries.iter(), |w, (name, entry)| {
            w.str(name.as_str());
            match entry {
                TreeEntry::Tree { id } => {
                    w.u8(TREE_ENTRY_TREE);
                    w.id(id.as_bytes());
                }
                TreeEntry::Blob { id, kind, size } => {
                    w.u8(TREE_ENTRY_BLOB);
                    w.id(id.as_bytes());
                    w.u8(kind.tag());
                    w.u64(*size);
                }
            }
        });
    }
}

impl Decode for Tree {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, CanonicalError> {
        r.magic(*TREE_MAGIC)?;
        r.version()?;
        let n = r.seq_len()?;
        let mut entries = BTreeMap::new();
        for _ in 0..n {
            let name = PathComponent::new(r.str()?)?;
            let entry = match r.u8()? {
                TREE_ENTRY_TREE => TreeEntry::Tree {
                    id: TreeId::from_bytes(r.id()?),
                },
                TREE_ENTRY_BLOB => {
                    let id = BlobId::from_bytes(r.id()?);
                    let kind = FileType::from_tag(r.u8()?)?;
                    let size = r.u64()?;
                    TreeEntry::Blob { id, kind, size }
                }
                tag => {
                    return Err(CanonicalError::BadTag {
                        what: "tree entry",
                        tag,
                    });
                }
            };
            insert_sorted(&mut entries, name, entry)?;
        }
        Ok(Tree { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree_encoding_is_fixed() {
        // magic + version + zero-length seq
        assert_eq!(
            Tree::empty().encode_to_vec(),
            b"SCTR\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00"
        );
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = Tree::empty().encode_to_vec();
        bytes.push(0);
        assert_eq!(Tree::decode(&bytes), Err(CanonicalError::TrailingBytes));
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        let mut bytes = Tree::empty().encode_to_vec();
        bytes[0] = b'X';
        assert!(matches!(
            Tree::decode(&bytes),
            Err(CanonicalError::BadMagic { .. })
        ));
        let mut bytes = Tree::empty().encode_to_vec();
        bytes[5] = 2;
        assert_eq!(
            Tree::decode(&bytes),
            Err(CanonicalError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn rejects_unsorted_tree_entries() {
        // Hand-build a tree with entries "b" then "a".
        let mut w = Writer::new();
        w.buf.extend_from_slice(TREE_MAGIC);
        w.u16(FORMAT_VERSION);
        w.u64(2);
        for name in ["b", "a"] {
            w.str(name);
            w.u8(TREE_ENTRY_TREE);
            w.id(&[0; 32]);
        }
        assert_eq!(
            Tree::decode(&w.into_vec()),
            Err(CanonicalError::UnsortedKeys)
        );
    }

    #[test]
    fn rejects_duplicate_tree_entries() {
        let mut w = Writer::new();
        w.buf.extend_from_slice(TREE_MAGIC);
        w.u16(FORMAT_VERSION);
        w.u64(2);
        for _ in 0..2 {
            w.str("a");
            w.u8(TREE_ENTRY_TREE);
            w.id(&[0; 32]);
        }
        assert_eq!(
            Tree::decode(&w.into_vec()),
            Err(CanonicalError::UnsortedKeys)
        );
    }

    #[test]
    fn rejects_invalid_path_in_changeset() {
        let mut w = Writer::new();
        w.buf.extend_from_slice(CHANGESET_MAGIC);
        w.u16(FORMAT_VERSION);
        w.str("repo");
        w.u64(0); // parents
        w.str("a");
        w.str("a@example.com");
        w.i64(0);
        w.i32(0);
        w.u8(0); // committer
        w.u8(0); // committer_date
        w.str("msg");
        w.u64(1); // file_changes
        w.str("../etc/passwd");
        w.u8(FILE_CHANGE_DELETED);
        w.u64(0); // extra
        assert!(matches!(
            Changeset::decode(&w.into_vec()),
            Err(CanonicalError::InvalidPath(_))
        ));
    }

    #[test]
    fn truncated_input_is_eof() {
        let bytes = Tree::empty().encode_to_vec();
        assert_eq!(
            Tree::decode(&bytes[..5]),
            Err(CanonicalError::UnexpectedEof)
        );
    }
}
