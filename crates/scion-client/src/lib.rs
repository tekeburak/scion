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
//!   every file, so `status` only rehashes files whose metadata changed;
//! * `added` — untracked paths `scn add` marked for the next commit, one per
//!   line. There are no ignore files: nothing untracked is committed unless
//!   it was added.
//!
//! Checkout starts from a root tree id, fetches the trees the sparse profile
//! reaches level by level, then the blobs, every request naming one immutable
//! object. The client never tells the server what it already has. In the MVP
//! the client talks to `scion-server` directly; the cache tiers come later.

#![forbid(unsafe_code)]

mod checkout;
mod commit;
mod index;
mod remote;
mod sparse;
mod status;
mod switch;
mod workspace;

pub use checkout::{CheckoutError, MAIN_REF, checkout, clone};
pub use commit::{CommitError, commit};
pub use index::{Index, IndexEntry};
pub use remote::{
    BlameRun, ChangeRequestStatus, DiffEntry, FileEntry, Merged, RPC_DEADLINE, Rebased, Remote,
    RemoteChangeRequest, RemoteError, RemoteRef, RequestStatus,
};
pub use sparse::{SparseError, SparseProfile};
pub use status::{FileStatus, StatusError, read_working_file, status};
pub use switch::{Marked, SwitchError, Updated, merge, switch, update, update_with_markers};
pub use workspace::{Config, Head, State, Workspace, WorkspaceError};
