//! `scion-client`: the library behind `scn`.
//!
//! A working copy is a directory with a `.scn/` folder holding:
//!
//! * `config` — server address and repository name (TOML);
//! * `state` — `current_head`, `base` and local head labels (TOML, see
//!   `docs/DATA_MODEL.md`);
//! * `sparse` — the sparse profile: one directory or file per line, empty
//!   for the whole repository;
//! * `index` — what is checked out: path, blob id, kind, size and mtime of
//!   every file, so `status` only rehashes files whose metadata changed.
//!
//! Checkout starts from a root tree id, fetches the trees the sparse profile
//! reaches level by level, then the blobs, every request naming one immutable
//! object. The client never tells the server what it already has. In the MVP
//! the client talks to `scion-server` directly; the cache tiers come later.

#![forbid(unsafe_code)]

mod checkout;
mod index;
mod remote;
mod sparse;
mod workspace;

pub use checkout::{CheckoutError, MAIN_REF, checkout, clone};
pub use index::{Index, IndexEntry};
pub use remote::{RPC_DEADLINE, Remote, RemoteError, RemoteRef};
pub use sparse::{SparseError, SparseProfile};
pub use workspace::{Config, Head, State, Workspace, WorkspaceError};
