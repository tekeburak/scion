//! Storage abstractions for Scion.
//!
//! Two independent stores, mirroring the article's split between Spanner
//! (metadata) and Cloud Storage (objects):
//!
//! * [`MetadataStore`]: changesets (the write-ahead log), refs (a single-row
//!   compare-and-swap each), and derived-data pointers.
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
    #[error("stored object is corrupt: {0}")]
    Corrupt(String),
    #[error("backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Result alias for store operations.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Changesets, refs and derived-data pointers.
#[async_trait]
pub trait MetadataStore: Send + Sync {
    /// Store a changeset. Idempotent: storing the same changeset twice is a
    /// no-op that returns the same id. All parents must already exist.
    async fn put_changeset(&self, cs: &Changeset) -> Result<ChangesetId>;

    /// Fetch a changeset by id.
    async fn get_changeset(&self, repo: &str, id: ChangesetId) -> Result<Option<Changeset>>;

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

    /// Record a derived-data pointer. Idempotent.
    async fn put_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
        pointer: Vec<u8>,
    ) -> Result<()>;

    /// Read a derived-data pointer.
    async fn get_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
    ) -> Result<Option<Vec<u8>>>;

    /// Record that git commit `git_sha` was imported as `cs`. Idempotent.
    async fn put_ingest_checkpoint(
        &self,
        repo: &str,
        git_sha: &[u8],
        cs: ChangesetId,
    ) -> Result<()>;

    /// Every git commit imported into `repo` so far, with its changeset.
    async fn ingest_checkpoints(&self, repo: &str) -> Result<Vec<(Vec<u8>, ChangesetId)>>;
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
