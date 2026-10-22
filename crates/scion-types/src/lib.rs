//! Core data model for Scion.
//!
//! This crate defines the *inherent* data of a repository — the things that are
//! content-addressed and form the Merkle DAG:
//!
//! * [`Changeset`]: a flat list of files changed relative to the first parent,
//!   plus commit metadata. Its [`ChangesetId`] is `blake3(canonical(changeset))`.
//! * [`Tree`]: a directory listing. Trees are *derived* from changesets by the
//!   server, never stored inside a changeset. `TreeId = blake3(canonical(tree))`.
//! * Blobs: raw file bytes. `BlobId = blake3(bytes)`.
//!
//! The byte-level canonical encoding lives in [`canonical`] and is specified in
//! `docs/DATA_MODEL.md`. It is deliberately *not* protobuf: hash stability must
//! never depend on wire-format evolution.
//!
//! Nothing in this crate performs I/O.

#![forbid(unsafe_code)]

pub mod canonical;
mod changeset;
mod id;
mod path;
mod tree;

pub use canonical::{CanonicalError, Decode, Encode};

/// The release this build belongs to: the workspace version, followed by the
/// commit when built with `SCION_COMMIT` set (as the release workflow does).
/// Every binary prints it for `--version`, and the server shows its own.
pub fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| match option_env!("SCION_COMMIT") {
        Some(commit) => format!("{} ({commit})", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").to_owned(),
    })
}

/// The per-blob size limit, in bytes (64 MiB): the largest blob any write
/// path accepts, whether it arrives over gRPC, the HTTP API or the MCP tools,
/// or from an import. A blob travels whole in one request; large-file
/// chunking is a non-goal.
pub const MAX_BLOB_BYTES: usize = 64 << 20;

/// The refusal of a blob of `len` bytes, over [`MAX_BLOB_BYTES`]; every write
/// path says the same.
#[must_use]
pub fn blob_too_large(len: usize) -> String {
    format!("a blob of {len} bytes is over the per-blob limit of {MAX_BLOB_BYTES} bytes (64 MiB)")
}

pub use changeset::{Changeset, FileChange, FileType, Identity, Timestamp};
pub use id::{BlobId, ChangesetId, IdParseError, TreeId};
pub use path::{PathComponent, PathError, RepoPath};
pub use tree::{Tree, TreeEntry};
