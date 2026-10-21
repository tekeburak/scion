//! Derived data.
//!
//! A changeset records only what changed. Everything else — directory trees,
//! file history, blame, ancestry indexes — is *derived*: computed from the
//! changeset and the same derived value for its parents, cached in the
//! metadata store, and recomputable at any time. Clients never see these
//! structures, so they can be replaced without a client change.
//!
//! [`Derivable`] is the one trait every derived type implements; [`Deriver`]
//! walks underived ancestors in topological order and fills the cache.
//! See `docs/ARCHITECTURE.md` § Derived data.

#![forbid(unsafe_code)]

mod blame;
mod deriver;
mod path_history;
mod root_tree;
mod skip_list;
#[cfg(any(test, feature = "test-util"))]
#[doc(hidden)]
pub mod test_util;

use scion_types::{BlobId, ChangesetId, TreeId};

pub use blame::{Blame, BlameNode, FileBlame};
pub use deriver::{Derivable, Deriver};
pub use path_history::{ANCHOR_STEP, Change, HistoryEntry, HistoryNode, NodeAt, PathHistory};
pub use root_tree::RootTree;
pub use skip_list::SkipList;

/// The decoders of derived objects read back from the stores, for
/// the fuzz targets in `scion-sim`.
#[doc(hidden)]
pub mod decode {
    use scion_types::BlobId;

    /// Node `index` of the history page `bytes`, if it decodes.
    #[must_use]
    pub fn history_node(bytes: &[u8], index: u32) -> Option<crate::HistoryNode> {
        crate::path_history::decode_node(bytes, BlobId::hash(bytes), index)
    }

    /// The blame node `bytes`, if it decodes, and its re-encoding.
    #[must_use]
    pub fn blame_node(bytes: &[u8]) -> Option<(crate::BlameNode, Vec<u8>)> {
        let node = crate::BlameNode::decode(bytes)?;
        let again = node.encode();
        Some((node, again))
    }
}

/// Error produced while deriving.
#[derive(Debug, thiserror::Error)]
pub enum DeriveError {
    #[error("changeset {0} not found")]
    MissingChangeset(ChangesetId),
    #[error("tree {0} not found in the metadata store")]
    MissingTree(TreeId),
    #[error("derived object {0} not found")]
    MissingObject(BlobId),
    #[error("derived object {0} is corrupt")]
    CorruptObject(BlobId),
    #[error("stored {ty} for {cs} is corrupt")]
    CorruptPointer { ty: &'static str, cs: ChangesetId },
    #[error(transparent)]
    Store(#[from] scion_store::StoreError),
}

/// Result alias for derivation.
pub type Result<T> = std::result::Result<T, DeriveError>;
