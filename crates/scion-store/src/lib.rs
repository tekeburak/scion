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
    #[error("repository {0:?} does not exist (create it with `scn init`)")]
    RepoNotFound(String),
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

/// One derived-data pointer of a repository: `(type, version, changeset,
/// pointer)`.
pub type DerivedPointer = (String, u32, ChangesetId, Vec<u8>);

/// Changesets, refs and derived-data pointers.
#[async_trait]
pub trait MetadataStore: Send + Sync {
    /// Create repository `name`; a no-op if it exists. Every other write
    /// fails with [`StoreError::RepoNotFound`] until this has run.
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
