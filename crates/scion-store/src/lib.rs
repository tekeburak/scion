//! Storage abstractions for Scion.
//!
//! Two independent stores, mirroring the article's split between Spanner
//! (metadata) and Cloud Storage (objects):
//!
//! * [`MetadataStore`]: changesets (the write-ahead log), refs (a single-row
//!   compare-and-swap each), derived-data pointers, change requests and
//!   access grants.
//! * [`ObjectStore`]: immutable, content-addressed blobs and trees.
//!
//! Implementations:
//!
//! | store | metadata | objects |
//! |---|---|---|
//! | [`MemoryStore`] | ✓ | ✓ |
//! | [`PostgresStore`] | ✓ | |
//! | [`ObjectStorage`] (S3, GCS, Azure, local fs, in-memory via `object_store`) | | ✓ |
//!
//! Every implementation must pass [`conformance`].

#![forbid(unsafe_code)]

pub mod conformance;
mod memory;
mod object;
mod postgres;
mod recent;
mod refs;

use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use scion_types::{BlobId, Changeset, ChangesetId, Tree, TreeId};

pub use memory::MemoryStore;
pub use object::{ObjectStorage, ZSTD_LEVEL};
pub use postgres::{MIGRATOR, PostgresStore};
pub use refs::{RefState, is_allowed_ref_name};

/// Error returned by store operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("repository {0:?} does not exist (create it with `scn init`)")]
    RepoNotFound(String),
    #[error("{0:?} is reserved and cannot name a repository")]
    ReservedRepoName(String),
    #[error("ref {0:?} already exists")]
    RefExists(String),
    #[error("ref {0:?} not found")]
    RefNotFound(String),
    #[error("ref name {0:?} is not allowed (only `main` and `release/*`)")]
    RefNameNotAllowed(String),
    #[error("compare-and-swap failed: expected version {expected}, found {actual}")]
    CasMismatch { expected: u64, actual: u64 },
    #[error("changeset {0} references unknown parent {1}")]
    UnknownParent(ChangesetId, ChangesetId),
    #[error("change request {0} not found")]
    ChangeRequestNotFound(u64),
    #[error("change request {id} is {status}, not landing")]
    ChangeRequestNotLanding {
        id: u64,
        status: ChangeRequestStatus,
    },
    #[error("stored object is corrupt: {0}")]
    Corrupt(String),
    #[error("backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Result alias for store operations.
pub type Result<T> = std::result::Result<T, StoreError>;

/// One derived-data pointer of a repository: `(type, version, changeset,
/// pointer)`.
pub type DerivedPointer = (String, u32, ChangesetId, Vec<u8>);

/// Where a change request is: `pending` → `landing` → `landed`,
/// `needs_rebase` or `rejected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeRequestStatus {
    /// Submitted, waiting for the land worker.
    Pending,
    /// Claimed by the land worker.
    Landing,
    /// On `main` as [`ChangeRequest::landed`].
    Landed,
    /// Conflicts with `main` at [`ChangeRequest::conflicts`].
    NeedsRebase,
    /// Rebased onto `main`, it would land nothing.
    Rejected,
}

impl ChangeRequestStatus {
    const ALL: [Self; 5] = [
        Self::Pending,
        Self::Landing,
        Self::Landed,
        Self::NeedsRebase,
        Self::Rejected,
    ];

    /// The name used in the schema.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Landing => "landing",
            Self::Landed => "landed",
            Self::NeedsRebase => "needs_rebase",
            Self::Rejected => "rejected",
        }
    }

    /// The status [`as_str`](Self::as_str) spells `s`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|st| st.as_str() == s)
    }
}

impl std::fmt::Display for ChangeRequestStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A request to land `head` on `main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRequest {
    /// Assigned at submission; increases in submission order.
    pub id: u64,
    /// The changeset to land.
    pub head: ChangesetId,
    /// `main`'s changeset when the request was submitted.
    pub base: ChangesetId,
    /// The principal that submitted it; the land worker lands only the
    /// paths this principal may change.
    pub submitter: String,
    /// Where the request is.
    pub status: ChangeRequestStatus,
    /// The changeset `main` moved to; set once `landed`.
    pub landed: Option<ChangesetId>,
    /// When `needs_rebase`, the paths that conflict with `main`; when
    /// `rejected`, the paths the submitter may not change, if any.
    pub conflicts: Vec<String>,
    /// When the request was recorded.
    pub submitted_at: SystemTime,
    /// When `main` moved to [`landed`](Self::landed); set once `landed`.
    pub landed_at: Option<SystemTime>,
}

/// How the land worker finished a request it claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandOutcome {
    /// `main` moved to this changeset.
    Landed(ChangesetId),
    /// The rebase conflicted at these paths.
    NeedsRebase(Vec<String>),
    /// The rebase would land nothing.
    Rejected,
    /// The change touches these paths, which the submitter may not change.
    Denied(Vec<String>),
}

impl LandOutcome {
    fn status(&self) -> ChangeRequestStatus {
        match self {
            Self::Landed(_) => ChangeRequestStatus::Landed,
            Self::NeedsRebase(_) => ChangeRequestStatus::NeedsRebase,
            Self::Rejected | Self::Denied(_) => ChangeRequestStatus::Rejected,
        }
    }
}

/// The reserved repository name under which server-wide grants are stored.
/// [`MetadataStore::create_repo`] refuses it and every other method treats it
/// as an unknown repository, except the grant methods.
pub const SERVER_GRANTS: &str = "*";

/// What a grant allows. Each role includes the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Read changesets, trees, blobs and derived data.
    Reader,
    /// Also create changesets, merges, reverts and change requests.
    Writer,
    /// Also move refs directly and manage grants.
    Admin,
}

impl Role {
    const ALL: [Self; 3] = [Self::Reader, Self::Writer, Self::Admin];

    /// The name used in the schema and on the command line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Writer => "writer",
            Self::Admin => "admin",
        }
    }

    /// The role [`as_str`](Self::as_str) spells `s`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One principal's role on the paths of a repository under `path_prefix`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// `github:<login>` or `sp:<name>`.
    pub principal: String,
    /// A directory of the repository, without a trailing `/`; empty for the
    /// whole repository.
    pub path_prefix: String,
    /// What the principal may do there.
    pub role: Role,
}

/// Changesets, refs, derived-data pointers, change requests and grants.
#[async_trait]
pub trait MetadataStore: Send + Sync {
    /// Create repository `name`; a no-op if it exists. Every other write
    /// fails with [`StoreError::RepoNotFound`] until this has run.
    /// [`SERVER_GRANTS`] is [`StoreError::ReservedRepoName`].
    async fn create_repo(&self, name: &str) -> Result<()>;

    /// Store changesets in order. Idempotent: storing a changeset again is a
    /// no-op. Every parent must already exist or come earlier in `batch`.
    async fn put_changesets(&self, batch: &[Changeset]) -> Result<()>;

    /// Store one changeset; see [`put_changesets`](Self::put_changesets).
    async fn put_changeset(&self, cs: &Changeset) -> Result<ChangesetId> {
        self.put_changesets(std::slice::from_ref(cs)).await?;
        Ok(cs.id())
    }

    /// Fetch a changeset by id.
    async fn get_changeset(&self, repo: &str, id: ChangesetId) -> Result<Option<Changeset>>;

    /// Fetch changesets by id, in the order of `ids`; one round trip where
    /// the backend allows.
    async fn get_changesets(
        &self,
        repo: &str,
        ids: &[ChangesetId],
    ) -> Result<Vec<Option<Changeset>>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.get_changeset(repo, *id).await?);
        }
        Ok(out)
    }

    /// The generation number of a changeset (`max(parent gen) + 1`, roots are
    /// 0), or `None` if unknown.
    async fn generation(&self, repo: &str, id: ChangesetId) -> Result<Option<u64>>;

    /// Create a ref. Fails if it exists or the name is not allowed.
    async fn create_ref(&self, repo: &str, name: &str, target: ChangesetId) -> Result<RefState>;

    /// Read a ref.
    async fn get_ref(&self, repo: &str, name: &str) -> Result<Option<RefState>>;

    /// Move a ref iff its current version equals `expected_version`.
    async fn move_ref(
        &self,
        repo: &str,
        name: &str,
        expected_version: u64,
        new_target: ChangesetId,
    ) -> Result<RefState>;

    /// All refs of a repository, sorted by name.
    async fn list_refs(&self, repo: &str) -> Result<Vec<(String, RefState)>>;

    /// Record derived-data pointers of `repo`. Idempotent.
    async fn put_derived_many(&self, repo: &str, pointers: Vec<DerivedPointer>) -> Result<()>;

    /// Record one derived-data pointer; see
    /// [`put_derived_many`](Self::put_derived_many).
    async fn put_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
        pointer: Vec<u8>,
    ) -> Result<()> {
        self.put_derived_many(repo, vec![(ty.to_owned(), version, cs, pointer)])
            .await
    }

    /// Read a derived-data pointer.
    async fn get_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
    ) -> Result<Option<Vec<u8>>>;

    /// Read the derived-data pointers of type `ty` at `version` for `ids`, in
    /// their order; one round trip where the backend allows.
    async fn get_derived_many(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        ids: &[ChangesetId],
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.get_derived(repo, ty, version, *id).await?);
        }
        Ok(out)
    }

    /// Record that each git commit `(git_sha, cs)` was imported as `cs`.
    /// Idempotent: the first record of a sha wins.
    async fn put_ingest_checkpoints(
        &self,
        repo: &str,
        done: &[(Vec<u8>, ChangesetId)],
    ) -> Result<()>;

    /// Every git commit imported into `repo` so far, with its changeset.
    async fn ingest_checkpoints(&self, repo: &str) -> Result<Vec<(Vec<u8>, ChangesetId)>>;

    /// Record a `pending` request by `submitter` to land `head`, submitted
    /// while `main` was at `base`.
    async fn submit_change_request(
        &self,
        repo: &str,
        head: ChangesetId,
        base: ChangesetId,
        submitter: &str,
    ) -> Result<ChangeRequest>;

    /// Read a change request of `repo`.
    async fn get_change_request(&self, repo: &str, id: u64) -> Result<Option<ChangeRequest>>;

    /// The newest `limit` requests of `repo`, only those in `status` if
    /// given, newest first. An unknown repository is
    /// [`StoreError::RepoNotFound`].
    async fn list_change_requests(
        &self,
        repo: &str,
        status: Option<ChangeRequestStatus>,
        limit: u32,
    ) -> Result<Vec<ChangeRequest>>;

    /// For each of `changesets` that a `landed` request of `repo` put on
    /// `main`, as its head or as its rebased [`landed`](ChangeRequest::landed),
    /// that request (the oldest, if several did), in the order asked;
    /// changesets no request landed are left out. An unknown repository is
    /// [`StoreError::RepoNotFound`].
    async fn find_landings(
        &self,
        repo: &str,
        changesets: &[ChangesetId],
    ) -> Result<Vec<(ChangesetId, ChangeRequest)>>;

    /// Move the oldest `pending` request of `repo` to `landing` and return
    /// it; `None` if nothing is pending. Concurrent claims never return the
    /// same request.
    async fn claim_next_change_request(&self, repo: &str) -> Result<Option<ChangeRequest>>;

    /// The oldest `landing` request of `repo`: one a stopped land worker
    /// claimed and did not finish.
    async fn landing_change_request(&self, repo: &str) -> Result<Option<ChangeRequest>>;

    /// Finish a `landing` request of `repo` with `outcome`. A request in any
    /// other status is [`StoreError::ChangeRequestNotLanding`] and stays as
    /// it is.
    async fn finish_change_request(
        &self,
        repo: &str,
        id: u64,
        outcome: LandOutcome,
    ) -> Result<ChangeRequest>;

    /// Reject a `pending` or `needs_rebase` request of `repo`, clearing its
    /// conflicts, and return it; a request in any other status is returned
    /// as it is. One the land worker claimed first stays `landing`.
    async fn reject_change_request(&self, repo: &str, id: u64) -> Result<ChangeRequest>;

    /// The repositories with a `pending` or `landing` request, sorted: the
    /// ones whose land worker has work when the server starts.
    async fn repos_with_open_change_requests(&self) -> Result<Vec<String>>;

    /// Record `grant` in `repo` (a repository or [`SERVER_GRANTS`]),
    /// replacing the role of an existing grant to the same principal and
    /// prefix.
    async fn put_grant(&self, repo: &str, grant: &Grant) -> Result<()>;

    /// Remove the grant of `principal` on `path_prefix` in `repo`; `false` if
    /// there was none.
    async fn delete_grant(&self, repo: &str, principal: &str, path_prefix: &str) -> Result<bool>;

    /// Every grant of `principal` in `repo`, by path prefix; empty for an
    /// unknown repository.
    async fn grants_for(&self, repo: &str, principal: &str) -> Result<Vec<Grant>>;

    /// Every grant in `repo`, by principal then path prefix; empty for an
    /// unknown repository.
    async fn grants(&self, repo: &str) -> Result<Vec<Grant>>;

    /// The repositories `principal` holds a grant in, by name; never
    /// [`SERVER_GRANTS`].
    async fn repos_of(&self, principal: &str) -> Result<Vec<String>>;
}

/// Immutable content-addressed objects.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Store a blob; returns `blake3(bytes)`. Idempotent.
    async fn put_blob(&self, bytes: Bytes) -> Result<BlobId>;

    /// Fetch a blob.
    async fn get_blob(&self, id: BlobId) -> Result<Option<Bytes>>;

    /// For each id, whether the blob is present. Same order as `ids`.
    async fn has_blobs(&self, ids: &[BlobId]) -> Result<Vec<bool>>;

    /// Store a tree; returns its id. Idempotent.
    async fn put_tree(&self, tree: &Tree) -> Result<TreeId>;

    /// Fetch a tree.
    async fn get_tree(&self, id: TreeId) -> Result<Option<Tree>>;
}
