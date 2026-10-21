//! The changeset: Scion's unit of work and the entry in the write-ahead log.

use std::collections::BTreeMap;

use crate::canonical::{self, Encode};
use crate::{BlobId, ChangesetId, RepoPath};

/// An author or committer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Identity {
    pub name: String,
    pub email: String,
}

impl Identity {
    /// Construct an identity.
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
        }
    }
}

/// A point in time with the author's UTC offset, like git.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Timestamp {
    /// Seconds since the Unix epoch, UTC.
    pub unix_secs: i64,
    /// Offset from UTC in minutes (e.g. `+03:00` is `180`).
    pub tz_offset_minutes: i32,
}

impl Timestamp {
    /// Construct a timestamp.
    #[must_use]
    pub const fn new(unix_secs: i64, tz_offset_minutes: i32) -> Self {
        Self {
            unix_secs,
            tz_offset_minutes,
        }
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// `YYYY-MM-DD HH:MM:SS +HHMM`, in the timestamp's own offset.
impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let offset = i64::from(self.tz_offset_minutes);
        let local = self.unix_secs + offset * 60;
        let (y, m, d) = civil_from_days(local.div_euclid(86_400));
        let secs = local.rem_euclid(86_400);
        let sign = if offset < 0 { '-' } else { '+' };
        write!(
            f,
            "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} {sign}{:02}{:02}",
            secs / 3600,
            secs / 60 % 60,
            secs % 60,
            offset.abs() / 60,
            offset.abs() % 60,
        )
    }
}

/// The kind of a tracked file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u8)]
pub enum FileType {
    Regular = 0,
    Executable = 1,
    Symlink = 2,
}

impl FileType {
    /// Decode from the canonical tag byte.
    pub(crate) fn from_tag(tag: u8) -> Result<Self, canonical::CanonicalError> {
        match tag {
            0 => Ok(Self::Regular),
            1 => Ok(Self::Executable),
            2 => Ok(Self::Symlink),
            other => Err(canonical::CanonicalError::BadTag {
                what: "file type",
                tag: other,
            }),
        }
    }

    /// The canonical tag byte.
    #[must_use]
    pub const fn tag(self) -> u8 {
        self as u8
    }
}

/// What happened to one path, relative to the changeset's first parent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FileChange {
    /// The path was removed.
    Deleted,
    /// The path was added or modified.
    Tracked {
        content: BlobId,
        kind: FileType,
        size: u64,
        /// Explicit copy/rename provenance (no heuristic rename detection).
        copy_from: Option<(RepoPath, ChangesetId)>,
    },
}

/// A changeset. See `docs/DATA_MODEL.md`.
///
/// Note the absence of a tree pointer and of a signature: trees are derived
/// data, and signatures are detached (stored next to the changeset, over its id).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Changeset {
    /// Repository name. Part of the hash: the same change in two repositories is
    /// two changesets.
    pub repo: String,
    /// Parent changesets. The first parent is the mainline.
    pub parents: Vec<ChangesetId>,
    pub author: Identity,
    pub author_date: Timestamp,
    pub committer: Option<Identity>,
    pub committer_date: Option<Timestamp>,
    pub message: String,
    /// Files changed relative to **the first parent only** (or to the empty tree
    /// for a root changeset).
    pub file_changes: BTreeMap<RepoPath, FileChange>,
    /// Opaque extension data, e.g. `git-sha` set by the ingestion pipeline.
    pub extra: BTreeMap<String, Vec<u8>>,
}

impl Changeset {
    /// The first parent, if any.
    #[must_use]
    pub fn first_parent(&self) -> Option<ChangesetId> {
        self.parents.first().copied()
    }

    /// The empty root changeset `scn init` creates a repository with. It
    /// depends only on `repo`, so every client builds the same one; storing
    /// it is the only write that creates a repository.
    #[must_use]
    pub fn repository_root(repo: &str) -> Self {
        Self {
            repo: repo.to_owned(),
            parents: Vec::new(),
            author: Identity::new("scion", ""),
            author_date: Timestamp::new(0, 0),
            committer: None,
            committer_date: None,
            message: "Create repository".to_owned(),
            file_changes: BTreeMap::new(),
            extra: BTreeMap::new(),
        }
    }

    /// Compute the content-addressed id: `blake3(canonical(self))`.
    #[must_use]
    pub fn id(&self) -> ChangesetId {
        ChangesetId::hash(&self.encode_to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::Timestamp;

    #[test]
    fn displays_dates_in_their_own_offset() {
        assert_eq!(
            Timestamp::new(0, 0).to_string(),
            "1970-01-01 00:00:00 +0000"
        );
        // 2026-10-14T09:12:00Z
        assert_eq!(
            Timestamp::new(1_791_969_120, 180).to_string(),
            "2026-10-14 12:12:00 +0300"
        );
        assert_eq!(
            Timestamp::new(951_782_400, -330).to_string(),
            "2000-02-28 18:30:00 -0530"
        );
    }
}
