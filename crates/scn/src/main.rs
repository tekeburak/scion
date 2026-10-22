//! `scn`: the Scion command-line client.
//!
//! Setup: `login` signs in to a server with GitHub and keeps its token for
//! later commands, and `whoami` shows who it names; `init` creates an empty
//! repository on a server; `clone` makes a working copy of one; `access`
//! lets the repository's admins grant, revoke and list roles in it.
//!
//! Reading: `clone`, `status`, `log`, `diff`, `blame`. Every history question
//! is an RPC to `scion-server`; the working copy is only needed for `status`
//! and for diffing local edits.
//!
//! Writing: `add` marks untracked files for the next commit (there are no
//! ignore files); `commit` creates a changeset on the server and makes it the
//! working copy's anonymous head; `land` submits that head as a change
//! request, which the server rebases onto `main` and lands, and moves the
//! working copy to the landed changeset; `requests` lists the repository's
//! change requests, newest first. `update` has the server rebase the
//! head onto `main` (or another revision) ahead of landing, which a conflict
//! needs (`--conflicts` writes conflict markers to resolve instead of
//! refusing); `merge` has the server merge another revision (a `release/*` fix,
//! say) into the head; `revert` has the server undo a changeset on the head;
//! `checkout` moves the working copy to any changeset.
//! There is no branch command: work is shared by changeset id.
//!
//! A revision is `@` (the working copy's current head), a 64-hex changeset
//! id, a local head label, or a server ref (`main`, `release/*`).
//!
//! For agents: `--json` makes any command print one JSON document on stdout,
//! described by `schemas/v1/<command>.json`; a failure prints
//! `schemas/v1/error.json` instead, whose `code` is stable.
//! `explain-conflict` names, per conflicting path, the changesets on each
//! side since the merge base.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use scion_client::{
    AccessGrant, ChangeRequestStatus, CheckoutError, CommitError, Config, FileStatus, LoginPoll,
    MAIN_REF, Marked, Rebased, Remote, RemoteChangeRequest, RemoteError, RequestStatus,
    SparseProfile, State, SwitchError, Workspace, WorkspaceError, read_working_file, status,
};
use scion_types::{Changeset, ChangesetId, FileChange, FileType, Identity, RepoPath, Timestamp};
use serde_json::{Value, json};

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

/// Hex digits of a changeset id shown where a full id would not fit.
const SHORT_ID: usize = 12;

#[derive(Parser)]
#[command(name = "scn", version = scion_types::version(), about = "Scion source control client")]
struct Cli {
    /// Print one JSON document (see `schemas/v1/`) instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in to a server with GitHub and keep its token for this user.
    Login {
        /// Server address, e.g. `http://127.0.0.1:7420`.
        server: String,
    },
    /// Show who the token for a server names, until when, and its roles in
    /// the working copy's repository.
    Whoami {
        /// Server address; defaults to the working copy's server.
        server: Option<String>,
    },
    /// Grant, revoke and list roles in the working copy's repository (its
    /// admins only).
    Access {
        #[command(subcommand)]
        action: AccessAction,
    },
    /// Create an empty repository on a server, with `main` at its root.
    Init {
        /// Server address, e.g. `http://127.0.0.1:7420`.
        server: String,
        /// Repository name.
        repo: String,
    },
    /// Create a working copy of a repository at its current `main`.
    Clone {
        /// Server address, e.g. `http://127.0.0.1:7420`.
        server: String,
        /// Repository name.
        repo: String,
        /// Directory to create; defaults to the repository name.
        dest: Option<PathBuf>,
        /// Materialize only this directory or file (repeatable); default is
        /// the whole repository.
        #[arg(long = "sparse", value_name = "PATH")]
        sparse: Vec<String>,
    },
    /// List files that differ from the checked-out head.
    Status,
    /// Show first-parent history, newest first.
    Log {
        /// Only changesets that touched this file or directory.
        path: Option<String>,
        /// Revision to start from.
        #[arg(short, long, default_value = "@")]
        rev: String,
        /// Most changesets to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
        /// Also show the change request that landed each changeset, and when.
        #[arg(long)]
        landed: bool,
    },
    /// Show local edits, or the changes between two revisions.
    Diff {
        /// Old revision; with `to`, diff two revisions on the server.
        #[arg(requires = "to")]
        from: Option<String>,
        /// New revision.
        to: Option<String>,
    },
    /// Show which changeset last changed each line of a file.
    Blame {
        path: String,
        /// Revision to blame at.
        #[arg(short, long, default_value = "@")]
        rev: String,
    },
    /// Mark untracked files (or every untracked file under a directory) for
    /// the next commit.
    Add {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Commit modified, deleted and added files as a new anonymous head.
    Commit {
        #[arg(short, long)]
        message: String,
        #[arg(long, env = "SCN_AUTHOR", value_name = "NAME <EMAIL>")]
        author: String,
        /// Also land the new head on `main`, as `scn land` does, in the same
        /// round trip.
        #[arg(long)]
        land: bool,
    },
    /// Land the current head on `main`: the server rebases it onto `main`
    /// and lands it, or names the paths that conflict.
    Land {
        /// Submit and print the request id without waiting for the outcome.
        #[arg(long, conflicts_with = "status")]
        no_wait: bool,
        /// Show a submitted request instead of landing.
        #[arg(long, value_name = "ID")]
        status: Option<u64>,
    },
    /// List the repository's change requests, newest first.
    Requests {
        /// Only requests in this status.
        #[arg(long, value_enum)]
        status: Option<StatusArg>,
        /// Most requests to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
    },
    /// Rebase the current head onto a revision on the server and move the
    /// working copy there; refused with local edits, and a conflict changes
    /// nothing.
    Update {
        /// Revision to rebase onto.
        #[arg(default_value = MAIN_REF)]
        rev: String,
        /// On a conflict, move to the revision anyway with your changes as
        /// local edits and conflict markers in the conflicting files; resolve
        /// them and `commit`.
        #[arg(long)]
        conflicts: bool,
    },
    /// Merge a revision into the current head on the server and move the
    /// working copy to the merge; refused with local edits, and a conflict
    /// changes nothing.
    Merge {
        /// Revision to merge, e.g. `release/1`.
        rev: String,
        /// Message of the merge changeset; defaults to `Merge <rev>`.
        #[arg(short, long)]
        message: Option<String>,
        #[arg(long, env = "SCN_AUTHOR", value_name = "NAME <EMAIL>")]
        author: String,
    },
    /// Undo a changeset on the current head on the server and move the
    /// working copy to the revert; refused with local edits, and a path the
    /// changeset changed that was changed again since changes nothing.
    Revert {
        /// Revision to undo; it must be in the current head's history.
        rev: String,
        /// Message of the revert changeset; defaults to `Revert "<subject>"`
        /// and the reverted id.
        #[arg(short, long)]
        message: Option<String>,
        #[arg(long, env = "SCN_AUTHOR", value_name = "NAME <EMAIL>")]
        author: String,
    },
    /// Move the working copy to a revision.
    Checkout {
        rev: String,
        /// Throw away local edits (added files stay, untracked) and replace
        /// untracked files in the way.
        #[arg(long)]
        force: bool,
    },
    /// Say why the current head does not rebase cleanly onto a revision:
    /// each conflicting path and the changesets on both sides that changed
    /// it since the merge base.
    ExplainConflict {
        /// Revision the head would be rebased onto.
        #[arg(default_value = MAIN_REF)]
        rev: String,
    },
}

#[derive(Subcommand)]
enum AccessAction {
    /// Give a principal a role, replacing its role on the same directory.
    Grant {
        /// `github:<login>`, `sp:<name>`, or `anyone` (`reader` only), which
        /// makes the repository, or the directory, public on the web.
        principal: String,
        /// `reader`, `writer` or `admin`.
        role: String,
        /// Limit the grant to this directory, from the repository root.
        #[arg(long, value_name = "DIR")]
        path: Option<String>,
    },
    /// Remove a principal's grant on the whole repository or a directory.
    Revoke {
        principal: String,
        /// The directory of the grant, from the repository root.
        #[arg(long, value_name = "DIR")]
        path: Option<String>,
    },
    /// List every grant in the repository.
    List,
}

/// A change-request status, as `scn requests --status` spells it.
#[derive(Clone, Copy, clap::ValueEnum)]
enum StatusArg {
    Pending,
    Landing,
    Landed,
    #[value(alias = "needs_rebase")]
    NeedsRebase,
    Rejected,
}

impl From<StatusArg> for ChangeRequestStatus {
    fn from(s: StatusArg) -> Self {
        match s {
            StatusArg::Pending => Self::Pending,
            StatusArg::Landing => Self::Landing,
            StatusArg::Landed => Self::Landed,
            StatusArg::NeedsRebase => Self::NeedsRebase,
            StatusArg::Rejected => Self::Rejected,
        }
    }
}

/// The path `arg` names relative to the current directory, as a path
/// relative to `root`; empty for `root` itself.
fn relative_path(root: &Path, arg: &str) -> Result<String> {
    let mut abs = PathBuf::new();
    for c in std::env::current_dir()?.join(arg).components() {
        match c {
            Component::ParentDir => {
                abs.pop();
            }
            Component::CurDir => {}
            c => abs.push(c),
        }
    }
    let rel = abs
        .strip_prefix(root)
        .map_err(|_| format!("{arg}: outside the working copy"))?;
    Ok(rel
        .to_str()
        .ok_or_else(|| format!("{arg}: not UTF-8"))?
        .to_owned())
}

/// `Name <email>`.
fn parse_identity(s: &str) -> Result<Identity> {
    let bad = || format!("author {s:?}: expected `Name <email>`");
    let (name, rest) = s.split_once('<').ok_or_else(bad)?;
    let email = rest.strip_suffix('>').ok_or_else(bad)?;
    let name = name.trim();
    if name.is_empty() || email.is_empty() {
        return Err(bad().into());
    }
    Ok(Identity::new(name, email))
}

/// Where a command reports: text, or with `--json` one JSON document that
/// the command writes once it is done.
struct Out {
    w: io::BufWriter<io::StdoutLock<'static>>,
    json: bool,
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.w.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

impl Out {
    /// Write `v` as the command's JSON document.
    fn emit(&mut self, v: &Value) -> io::Result<()> {
        serde_json::to_writer(&mut self.w, v)?;
        writeln!(self.w)
    }
}

/// A refusal with the stable code `--json` reports it under, and the paths
/// it is about.
#[derive(Debug)]
struct Failure {
    code: &'static str,
    message: String,
    paths: Vec<RepoPath>,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            paths: Vec::new(),
        }
    }

    fn with_paths(mut self, paths: Vec<RepoPath>) -> Self {
        self.paths = paths;
        self
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

/// The `error.json` code of a gRPC status: the code's name, as the HTTP API
/// spells it.
fn rpc_code(code: tonic::Code) -> &'static str {
    use tonic::Code as C;
    match code {
        C::InvalidArgument | C::OutOfRange => "invalid_argument",
        C::Unauthenticated => "unauthenticated",
        C::PermissionDenied => "permission_denied",
        C::NotFound => "not_found",
        C::AlreadyExists => "already_exists",
        C::FailedPrecondition | C::Aborted => "failed_precondition",
        C::Unavailable => "unavailable",
        C::DeadlineExceeded => "deadline_exceeded",
        C::ResourceExhausted => "resource_exhausted",
        _ => "internal",
    }
}

fn remote_code(e: &RemoteError) -> &'static str {
    match e {
        RemoteError::Connect(..) => "unavailable",
        RemoteError::CertFile(..) => "local",
        RemoteError::Rpc(st) => rpc_code(st.code()),
        RemoteError::Malformed(_) => "internal",
        RemoteError::Token(..) | RemoteError::BadToken(..) => "unauthenticated",
    }
}

fn workspace_code(e: &WorkspaceError) -> &'static str {
    match e {
        WorkspaceError::NotFound(_) => "not_a_working_copy",
        WorkspaceError::AlreadyExists(_) => "already_exists",
        _ => "local",
    }
}

/// The `error.json` code of a failed command, and the paths it is about.
fn classify(e: &(dyn std::error::Error + 'static)) -> (&'static str, Vec<RepoPath>) {
    if let Some(f) = e.downcast_ref::<Failure>() {
        return (f.code, f.paths.clone());
    }
    if let Some(e) = e.downcast_ref::<RemoteError>() {
        return (remote_code(e), Vec::new());
    }
    if let Some(e) = e.downcast_ref::<WorkspaceError>() {
        return (workspace_code(e), Vec::new());
    }
    let checkout = |e: &CheckoutError| match e {
        CheckoutError::Remote(e) => remote_code(e),
        CheckoutError::Workspace(e) => workspace_code(e),
        CheckoutError::NotEmpty(_) => "already_exists",
        CheckoutError::Io(..) => "local",
    };
    if let Some(e) = e.downcast_ref::<CheckoutError>() {
        return (checkout(e), Vec::new());
    }
    if let Some(e) = e.downcast_ref::<SwitchError>() {
        return match e {
            SwitchError::Remote(e) => (remote_code(e), Vec::new()),
            SwitchError::Workspace(e) => (workspace_code(e), Vec::new()),
            SwitchError::Checkout(e) => (checkout(e), Vec::new()),
            SwitchError::Status(_) => ("local", Vec::new()),
            SwitchError::Dirty(paths) => ("dirty", paths.clone()),
            SwitchError::Untracked(paths) => ("untracked_in_the_way", paths.clone()),
            SwitchError::Conflict { paths, .. } => ("conflict", paths.clone()),
            SwitchError::ChangedSince { paths, .. } => ("changed_since", paths.clone()),
        };
    }
    if let Some(e) = e.downcast_ref::<CommitError>() {
        return match e {
            CommitError::Remote(e) => (remote_code(e), Vec::new()),
            CommitError::Workspace(e) => (workspace_code(e), Vec::new()),
            CommitError::NothingToCommit => ("nothing_to_commit", Vec::new()),
            CommitError::Unresolved(paths) => ("unresolved_conflicts", paths.clone()),
            CommitError::Status(_) | CommitError::Io(..) => ("local", Vec::new()),
        };
    }
    ("local", Vec::new())
}

/// Seconds since the Unix epoch.
fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Microseconds since the Unix epoch.
fn unix_micros(t: SystemTime) -> u64 {
    let micros = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_micros();
    u64::try_from(micros).unwrap_or(u64::MAX)
}

fn paths_json(paths: &[RepoPath]) -> Value {
    paths.iter().map(RepoPath::as_str).collect()
}

fn date_json(t: Timestamp) -> Value {
    json!({"unix_secs": t.unix_secs, "tz_offset_minutes": t.tz_offset_minutes})
}

/// A changeset in the HTTP API's changeset JSON (`docs/API.md`), with its id.
fn changeset_json(cs: &Changeset) -> Value {
    let person = |p: &Identity| json!({"name": p.name, "email": p.email});
    let changes: serde_json::Map<String, Value> = cs
        .file_changes
        .iter()
        .map(|(path, change)| {
            let change = match change {
                FileChange::Deleted => json!({"change": "deleted"}),
                FileChange::Tracked {
                    content,
                    kind,
                    size,
                    copy_from,
                } => {
                    let mut v = json!({
                        "change": "tracked",
                        "content": content.to_hex(),
                        "kind": kind_name(*kind),
                        "size": size,
                    });
                    if let Some((from, at)) = copy_from {
                        v["copy_from"] = json!({"path": from.as_str(), "changeset": at.to_hex()});
                    }
                    v
                }
            };
            (path.as_str().to_owned(), change)
        })
        .collect();
    let mut v = json!({
        "id": cs.id().to_hex(),
        "parents": cs.parents.iter().map(ChangesetId::to_hex).collect::<Vec<_>>(),
        "author": person(&cs.author),
        "author_date": date_json(cs.author_date),
        "message": cs.message,
        "file_changes": changes,
        "extra": cs.extra.iter().map(|(k, v)| (k.clone(), Value::from(hex::encode(v))))
            .collect::<serde_json::Map<_, _>>(),
    });
    if let Some(c) = &cs.committer {
        v["committer"] = person(c);
    }
    if let Some(d) = cs.committer_date {
        v["committer_date"] = date_json(d);
    }
    v
}

/// A change request in the HTTP API's change request JSON (`docs/API.md`).
fn request_json(cr: &RemoteChangeRequest) -> Value {
    let (status, landed, paths) = match &cr.status {
        RequestStatus::Pending => ("pending", None, &[][..]),
        RequestStatus::Landing => ("landing", None, &[][..]),
        RequestStatus::Landed(cs) => ("landed", Some(*cs), &[][..]),
        RequestStatus::NeedsRebase(paths) => ("needs_rebase", None, paths.as_slice()),
        RequestStatus::Rejected(paths) => ("rejected", None, paths.as_slice()),
    };
    let mut v = json!({
        "id": cr.id,
        "head": cr.head.to_hex(),
        "base": cr.base.to_hex(),
        "status": status,
        "conflicts": paths_json(paths),
        "submitted_unix_micros": unix_micros(cr.submitted_at),
    });
    if let Some(cs) = landed {
        v["landed"] = cs.to_hex().into();
    }
    if let Some(at) = cr.landed_at {
        v["landed_unix_micros"] = unix_micros(at).into();
    }
    v
}

/// An opened working copy and a connection to its server.
struct Session {
    ws: Workspace,
    config: Config,
    state: State,
    remote: Remote,
}

impl Session {
    async fn open() -> Result<Self> {
        let ws = Workspace::find(&std::env::current_dir()?)?;
        let config = ws.read_config()?;
        let state = ws.read_state()?;
        let remote = Remote::connect(&config.server).await?;
        Ok(Self {
            ws,
            config,
            state,
            remote,
        })
    }

    /// The changeset `rev` names.
    async fn resolve(&mut self, rev: &str) -> Result<ChangesetId> {
        if rev == "@" {
            return Ok(self.state.current_head);
        }
        if let Ok(id) = rev.parse() {
            return Ok(id);
        }
        if let Some(head) = self.state.heads.get(rev) {
            return Ok(head.head);
        }
        Ok(self.remote.get_ref(&self.config.repo, rev).await?.target)
    }

    /// The repository path `arg` names, relative to the current directory.
    fn repo_path(&self, arg: &str) -> Result<RepoPath> {
        let rel = relative_path(self.ws.root(), arg)?;
        Ok(RepoPath::new(rel).map_err(|e| format!("{arg}: {e}"))?)
    }
}

fn kind_name(kind: FileType) -> &'static str {
    match kind {
        FileType::Regular => "regular",
        FileType::Executable => "executable",
        FileType::Symlink => "symlink",
    }
}

/// `t` as a [`Timestamp`] displays it, in UTC.
fn format_utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    Timestamp::new(i64::try_from(secs).unwrap_or(i64::MAX), 0).to_string()
}

fn print_changeset(
    out: &mut impl Write,
    cs: &Changeset,
    landing: Option<&RemoteChangeRequest>,
) -> io::Result<()> {
    writeln!(out, "changeset {}", cs.id())?;
    if cs.parents.len() > 1 {
        let parents: Vec<String> = cs.parents.iter().map(ChangesetId::to_hex).collect();
        writeln!(out, "Parents: {}", parents.join(" "))?;
    }
    writeln!(out, "Author: {} <{}>", cs.author.name, cs.author.email)?;
    writeln!(out, "Date:   {}", cs.author_date)?;
    if let Some(cr) = landing {
        let at = cr.landed_at.map_or_else(|| "?".to_owned(), format_utc);
        writeln!(out, "Landed: {at} (request {})", cr.id)?;
    }
    writeln!(out)?;
    for line in cs.message.lines() {
        writeln!(out, "    {line}")?;
    }
    writeln!(out)
}

/// One side of a file diff: its kind and bytes.
type Side = Option<(FileType, Vec<u8>)>;

/// A unified diff of one file; either side may be absent.
fn print_file_diff(
    out: &mut impl Write,
    path: &RepoPath,
    old: &Side,
    new: &Side,
) -> io::Result<()> {
    writeln!(out, "diff a/{path} b/{path}")?;
    match (old, new) {
        (None, Some((kind, _))) => writeln!(out, "new {} file", kind_name(*kind))?,
        (Some((kind, _)), None) => writeln!(out, "deleted {} file", kind_name(*kind))?,
        (Some((a, _)), Some((b, _))) if a != b => {
            writeln!(out, "kind {} -> {}", kind_name(*a), kind_name(*b))?;
        }
        _ => {}
    }
    out.write_all(&unified(path, old, new))
}

/// The unified diff of one file's bytes; empty when they are equal.
fn unified(path: &RepoPath, old: &Side, new: &Side) -> Vec<u8> {
    let old_bytes = old.as_ref().map_or(&[][..], |(_, d)| d.as_slice());
    let new_bytes = new.as_ref().map_or(&[][..], |(_, d)| d.as_slice());
    if old_bytes == new_bytes {
        return Vec::new();
    }
    let name = |side: &Side, prefix: &str| -> Cow<'static, str> {
        match side {
            Some(_) => format!("{prefix}/{path}").into(),
            None => "/dev/null".into(),
        }
    };
    diffy::DiffOptions::new()
        .set_original_filename(name(old, "a"))
        .set_modified_filename(name(new, "b"))
        .create_patch_bytes(old_bytes, new_bytes)
        .to_bytes()
}

/// One file of `diff.json`.
fn file_diff_json(path: &RepoPath, old: &Side, new: &Side) -> Value {
    let side = |s: &Side| match s {
        Some((kind, data)) => json!({"kind": kind_name(*kind), "size": data.len()}),
        None => Value::Null,
    };
    json!({
        "path": path.as_str(),
        "old": side(old),
        "new": side(new),
        "patch": String::from_utf8_lossy(&unified(path, old, new)),
    })
}

/// Write `files` (in `diff.json` form) or each file's text diff.
fn report_diff(out: &mut Out, files: &[(RepoPath, Side, Side)]) -> io::Result<()> {
    if out.json {
        let files: Vec<Value> = files
            .iter()
            .map(|(p, old, new)| file_diff_json(p, old, new))
            .collect();
        return out.emit(&json!({"files": files}));
    }
    for (path, old, new) in files {
        print_file_diff(out, path, old, new)?;
    }
    Ok(())
}

async fn login(out: &mut Out, server: &str) -> Result<()> {
    // The server's own refusals (no GitHub login, denied, expired) are
    // sentences; show them without the status around them.
    let plain = |e: RemoteError| -> Box<dyn std::error::Error> {
        match e {
            RemoteError::Rpc(st)
                if matches!(
                    st.code(),
                    tonic::Code::FailedPrecondition
                        | tonic::Code::PermissionDenied
                        | tonic::Code::DeadlineExceeded
                ) =>
            {
                Failure::new(rpc_code(st.code()), st.message()).into()
            }
            e => e.into(),
        }
    };
    let mut remote = Remote::connect(server).await?;
    let start = remote.start_login().await.map_err(plain)?;
    let prompt = format!(
        "open {} and enter the code {}",
        start.verification_uri, start.user_code
    );
    // With `--json` stdout carries only the result, so the code goes to the
    // person at the terminal.
    if out.json {
        eprintln!("{prompt}");
    } else {
        writeln!(out, "{prompt}")?;
        out.flush()?;
    }
    let mut interval = start.interval;
    let (token, principal, expires_at) = loop {
        tokio::time::sleep(interval).await;
        match remote
            .finish_login(&start.device_code)
            .await
            .map_err(plain)?
        {
            LoginPoll::Pending { interval: next } if !next.is_zero() => interval = next,
            LoginPoll::Pending { .. } => {}
            LoginPoll::Done {
                token,
                principal,
                expires_at,
            } => break (token, principal, expires_at),
        }
    };
    let path = scion_client::save_token(server, &token)
        .map_err(|e| format!("cannot save the token for {server}: {e}"))?;
    if out.json {
        out.emit(&json!({
            "server": server,
            "principal": principal,
            "expires_unix_seconds": unix_secs(expires_at),
            "token_path": path.display().to_string(),
        }))?;
        return Ok(());
    }
    let expires = expires_at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    writeln!(
        out,
        "logged in to {server} as {principal} until {}; token in {}",
        Timestamp::new(expires, 0),
        path.display()
    )?;
    Ok(())
}

/// `<repo>`, or `<repo>:<dir>/` for a directory of it.
fn scope(repo: &str, dir: &str) -> String {
    match dir.trim_end_matches('/') {
        "" => repo.to_owned(),
        dir => format!("{repo}:{dir}/"),
    }
}

/// `<role> on <scope>`.
fn grant_line(repo: &str, g: &AccessGrant) -> String {
    format!("{} on {}", g.role, scope(repo, &g.path_prefix))
}

/// A grant in the HTTP API's form: `{"principal", "path", "role"}`.
fn grant_json(g: &AccessGrant) -> Value {
    json!({"principal": g.principal, "path": g.path_prefix, "role": g.role})
}

async fn whoami(out: &mut Out, server: Option<&str>) -> Result<()> {
    let (server, repo) = if let Some(s) = server {
        (s.to_owned(), String::new())
    } else {
        let config = Workspace::find(&std::env::current_dir()?)?.read_config()?;
        (config.server, config.repo)
    };
    let me = Remote::connect(&server).await?.who_am_i(&repo).await?;
    if out.json {
        out.emit(&json!({
            "server": server,
            "repo": (!repo.is_empty()).then_some(&repo),
            "principal": me.principal,
            "expires_unix_seconds": unix_secs(me.expires_at),
            "grants": me.grants.iter().map(grant_json).collect::<Vec<_>>(),
        }))?;
        return Ok(());
    }
    writeln!(
        out,
        "{} on {server}, token valid until {}",
        me.principal,
        format_utc(me.expires_at)
    )?;
    if repo.is_empty() {
        return Ok(());
    }
    if me.grants.is_empty() {
        writeln!(out, "no role in {repo}")?;
    }
    for g in &me.grants {
        writeln!(out, "{}", grant_line(&repo, g))?;
    }
    Ok(())
}

async fn access(out: &mut Out, action: AccessAction) -> Result<()> {
    let mut s = Session::open().await?;
    let repo = s.config.repo.clone();
    // The server's refusals are sentences; show them without the status
    // around them.
    let plain = |e: RemoteError| -> Box<dyn std::error::Error> {
        match e {
            RemoteError::Rpc(st)
                if matches!(
                    st.code(),
                    tonic::Code::InvalidArgument
                        | tonic::Code::NotFound
                        | tonic::Code::PermissionDenied
                ) =>
            {
                Failure::new(rpc_code(st.code()), st.message()).into()
            }
            e => e.into(),
        }
    };
    match action {
        AccessAction::Grant {
            principal,
            role,
            path,
        } => {
            let prefix = path.unwrap_or_default();
            s.remote
                .grant(&repo, &principal, &role, &prefix)
                .await
                .map_err(plain)?;
            if out.json {
                out.emit(&json!({"principal": principal, "path": prefix, "role": role}))?;
                return Ok(());
            }
            writeln!(
                out,
                "granted {principal} {role} on {}",
                scope(&repo, &prefix)
            )?;
        }
        AccessAction::Revoke { principal, path } => {
            let prefix = path.unwrap_or_default();
            s.remote
                .revoke(&repo, &principal, &prefix)
                .await
                .map_err(plain)?;
            if out.json {
                out.emit(&json!({"principal": principal, "path": prefix}))?;
                return Ok(());
            }
            writeln!(out, "revoked {principal} on {}", scope(&repo, &prefix))?;
        }
        AccessAction::List => {
            let grants = s.remote.list_grants(&repo).await.map_err(plain)?;
            if out.json {
                let grants: Vec<Value> = grants.iter().map(grant_json).collect();
                out.emit(&json!({"grants": grants}))?;
                return Ok(());
            }
            for g in grants {
                writeln!(out, "{} {}", g.principal, grant_line(&repo, &g))?;
            }
        }
    }
    Ok(())
}

async fn init(out: &mut Out, server: &str, repo: &str) -> Result<()> {
    let mut remote = Remote::connect(server).await?;
    let root = match remote.init_repo(repo).await {
        Ok(root) => root,
        Err(RemoteError::Rpc(st)) if st.code() == tonic::Code::AlreadyExists => {
            return Err(Failure::new(
                "already_exists",
                format!("repository {repo} already exists on {server}"),
            )
            .into());
        }
        Err(e) => return Err(e.into()),
    };
    if out.json {
        out.emit(&json!({"server": server, "repo": repo, "main": root.to_hex()}))?;
        return Ok(());
    }
    writeln!(
        out,
        "created {repo} on {server} ({MAIN_REF} at {root}); next: scn clone {server} {repo}"
    )?;
    Ok(())
}

async fn clone(
    out: &mut Out,
    server: &str,
    repo: &str,
    dest: Option<PathBuf>,
    sparse: &[String],
) -> Result<()> {
    let dest = dest.unwrap_or_else(|| PathBuf::from(repo));
    let paths = sparse
        .iter()
        .map(|p| RepoPath::new(p.trim_end_matches('/')).map_err(|e| format!("--sparse {p}: {e}")))
        .collect::<Result<_, _>>()?;
    let remote = Remote::connect(server).await?;
    let ws = scion_client::clone(remote, server, repo, &dest, SparseProfile::new(paths)).await?;
    let head = ws.read_state()?.current_head;
    let files = ws.read_index()?.entries.len();
    if out.json {
        out.emit(&json!({
            "server": server,
            "repo": repo,
            "head": head.to_hex(),
            "path": dest.display().to_string(),
            "files": files,
        }))?;
        return Ok(());
    }
    writeln!(
        out,
        "cloned {repo} at {head} into {} ({files} files)",
        dest.display()
    )?;
    Ok(())
}

fn status_name(st: FileStatus) -> &'static str {
    match st {
        FileStatus::Modified => "modified",
        FileStatus::Deleted => "deleted",
        FileStatus::Added => "added",
        FileStatus::Untracked => "untracked",
    }
}

fn print_status(out: &mut Out) -> Result<()> {
    let ws = Workspace::find(&std::env::current_dir()?)?;
    let changes = status(ws.root(), &ws.read_index()?, &ws.read_added()?)?;
    if out.json {
        let files: Vec<Value> = changes
            .iter()
            .map(|(path, st)| json!({"path": path.as_str(), "status": status_name(*st)}))
            .collect();
        out.emit(&json!({"files": files}))?;
        return Ok(());
    }
    for (path, st) in changes {
        writeln!(out, "{} {path}", st.code())?;
    }
    Ok(())
}

async fn log(out: &mut Out, path: Option<&str>, rev: &str, limit: u32, landed: bool) -> Result<()> {
    let mut s = Session::open().await?;
    let head = s.resolve(rev).await?;
    let repo = s.config.repo.clone();
    let changesets = match path {
        None => s.remote.log(&repo, head, limit).await?,
        Some(p) => {
            let path = s.repo_path(p)?;
            s.remote.path_log(&repo, head, &path, limit).await?
        }
    };
    let landings: HashMap<ChangesetId, RemoteChangeRequest> = if landed {
        let ids: Vec<ChangesetId> = changesets.iter().map(Changeset::id).collect();
        s.remote
            .find_landings(&repo, &ids)
            .await?
            .into_iter()
            .collect()
    } else {
        HashMap::new()
    };
    if out.json {
        let listed: Vec<Value> = changesets
            .iter()
            .map(|cs| {
                let mut v = changeset_json(cs);
                if landed {
                    v["landed_by"] = landings.get(&cs.id()).map_or(Value::Null, request_json);
                }
                v
            })
            .collect();
        out.emit(&json!({"changesets": listed}))?;
        return Ok(());
    }
    for cs in &changesets {
        print_changeset(out, cs, landings.get(&cs.id()))?;
    }
    Ok(())
}

/// Local edits against the checked-out head, as the next commit would record
/// them; untracked files are left out.
async fn diff_working_copy(out: &mut Out) -> Result<()> {
    let mut s = Session::open().await?;
    let index = s.ws.read_index()?;
    let mut files = Vec::new();
    for (path, st) in status(s.ws.root(), &index, &s.ws.read_added()?)? {
        let old = match st {
            FileStatus::Untracked => continue,
            FileStatus::Added => None,
            FileStatus::Modified | FileStatus::Deleted => {
                let e = index.entries[&path];
                Some((e.kind, s.remote.get_blob(&s.config.repo, e.id).await?))
            }
        };
        let new = match st {
            FileStatus::Modified | FileStatus::Added => {
                Some(read_working_file(&s.ws.root().join(path.as_str()))?)
            }
            _ => None,
        };
        files.push((path, old, new));
    }
    report_diff(out, &files)?;
    Ok(())
}

async fn diff_revisions(out: &mut Out, from: &str, to: &str) -> Result<()> {
    let mut s = Session::open().await?;
    let old = s.resolve(from).await?;
    let new = s.resolve(to).await?;
    let repo = s.config.repo.clone();
    let mut files = Vec::new();
    for e in s.remote.diff(&repo, Some(old), new).await? {
        let old = match e.old {
            Some(f) => Some((f.kind, s.remote.get_blob(&repo, f.id).await?)),
            None => None,
        };
        let new = match e.new {
            Some(f) => Some((f.kind, s.remote.get_blob(&repo, f.id).await?)),
            None => None,
        };
        files.push((e.path, old, new));
    }
    report_diff(out, &files)?;
    Ok(())
}

async fn blame(out: &mut Out, path: &str, rev: &str) -> Result<()> {
    let mut s = Session::open().await?;
    let cs = s.resolve(rev).await?;
    let path = s.repo_path(path)?;
    let repo = s.config.repo.clone();
    // The attribution and the file's content are fetched at once; the newest
    // changeset that touched the file holds its content.
    let mut other = s.remote.clone();
    let (runs, data) = tokio::try_join!(
        async { Ok(s.remote.blame(&repo, cs, &path).await?) },
        async {
            let last = other.path_log(&repo, cs, &path, 1).await?;
            let Some(FileChange::Tracked { content, .. }) =
                last.first().and_then(|c| c.file_changes.get(&path))
            else {
                return Err(format!("server history has no content for {path}").into());
            };
            Ok::<_, Box<dyn std::error::Error>>(other.get_blob(&repo, *content).await?)
        },
    )?;
    let mut lines = data.split_inclusive(|b| *b == b'\n');
    if out.json {
        let mut listed = Vec::new();
        for run in runs {
            let changeset = run.changeset.to_hex();
            for _ in 0..run.lines {
                let line = lines.next().ok_or("blame is longer than the file")?;
                listed.push(json!({
                    "changeset": changeset,
                    "line": String::from_utf8_lossy(line.strip_suffix(b"\n").unwrap_or(line)),
                }));
            }
        }
        out.emit(&json!({"path": path.as_str(), "changeset": cs.to_hex(), "lines": listed}))?;
        return Ok(());
    }
    for run in runs {
        let short = &run.changeset.to_hex()[..SHORT_ID];
        for _ in 0..run.lines {
            let line = lines.next().ok_or("blame is longer than the file")?;
            write!(out, "{short} ")?;
            out.write_all(line)?;
            if !line.ends_with(b"\n") {
                writeln!(out)?;
            }
        }
    }
    Ok(())
}

fn add(out: &mut Out, args: &[String]) -> Result<()> {
    let ws = Workspace::find(&std::env::current_dir()?)?;
    let mut added = ws.read_added()?;
    let mut marked = Vec::new();
    let changes = status(ws.root(), &ws.read_index()?, &added)?;
    for arg in args {
        let prefix = relative_path(ws.root(), arg)?;
        if std::fs::symlink_metadata(ws.root().join(&prefix)).is_err() {
            return Err(format!("{arg}: no such file or directory").into());
        }
        let under = |p: &RepoPath| {
            prefix.is_empty()
                || p.as_str()
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        };
        for (path, st) in &changes {
            if *st == FileStatus::Untracked && under(path) && added.insert(path.clone()) {
                marked.push(path.clone());
            }
        }
    }
    ws.write_added(&added)?;
    if out.json {
        out.emit(&json!({"added": paths_json(&marked)}))?;
        return Ok(());
    }
    for path in marked {
        writeln!(out, "A {path}")?;
    }
    Ok(())
}

/// The current time, in UTC.
fn now() -> Result<Timestamp> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    Ok(Timestamp::new(i64::try_from(secs)?, 0))
}

async fn commit(out: &mut Out, message: &str, author: &str, land: bool) -> Result<()> {
    let author = parse_identity(author)?;
    let date = now()?;
    let mut s = Session::open().await?;
    if land {
        let cr = scion_client::commit_and_land(&s.ws, &s.remote, author, date, message).await?;
        s.state = s.ws.read_state()?;
        return landed(out, s, cr).await;
    }
    let id = scion_client::commit(&s.ws, &s.remote, author, date, message).await?;
    if out.json {
        out.emit(&json!({"head": id.to_hex()}))?;
        return Ok(());
    }
    writeln!(out, "committed {id}")?;
    Ok(())
}

/// `paths`, one per line, indented.
fn path_list(paths: &[RepoPath]) -> String {
    paths.iter().fold(String::new(), |mut out, p| {
        out.push_str("\n  ");
        out.push_str(p.as_str());
        out
    })
}

/// One line (plus conflicting paths) saying where `cr` is.
fn describe(cr: &RemoteChangeRequest) -> String {
    let (id, head) = (cr.id, cr.head);
    match &cr.status {
        RequestStatus::Pending => format!("request {id}: pending, {head}"),
        RequestStatus::Landing => format!("request {id}: landing, {head}"),
        RequestStatus::Landed(landed) if *landed == head => {
            format!("request {id}: landed {head} on {MAIN_REF}")
        }
        RequestStatus::Landed(landed) => {
            format!("request {id}: landed {head} on {MAIN_REF} as {landed}")
        }
        RequestStatus::NeedsRebase(paths) => format!(
            "request {id}: needs rebase, {head} conflicts with {MAIN_REF} in:{}",
            path_list(paths)
        ),
        RequestStatus::Rejected(paths) if paths.is_empty() => {
            format!("request {id}: rejected, {head} would change nothing on {MAIN_REF}")
        }
        RequestStatus::Rejected(paths) => format!(
            "request {id}: rejected, its submitter may not change:{}",
            path_list(paths)
        ),
    }
}

async fn land(out: &mut Out, no_wait: bool) -> Result<()> {
    let mut s = Session::open().await?;
    let repo = s.config.repo.clone();
    let head = s.state.current_head;
    let cr = s
        .remote
        .submit_change_request(&repo, head, !no_wait)
        .await?;
    if no_wait {
        if out.json {
            out.emit(&json!({"request": request_json(&cr), "head": head.to_hex()}))?;
            return Ok(());
        }
        writeln!(
            out,
            "submitted request {} for {head}; follow it with: scn land --status {}",
            cr.id, cr.id
        )?;
        return Ok(());
    }
    landed(out, s, cr).await
}

/// Follow submitted request `cr` for the current head of `s` to its end,
/// and move the working copy to what landed.
async fn landed(out: &mut Out, mut s: Session, mut cr: RemoteChangeRequest) -> Result<()> {
    let repo = s.config.repo.clone();
    let head = s.state.current_head;
    // A long queue answers before the request is finished; ask again.
    while !cr.status.is_finished() {
        cr = s.remote.get_change_request(&repo, cr.id, true).await?;
    }
    let landed = match &cr.status {
        RequestStatus::Landed(landed) => *landed,
        RequestStatus::NeedsRebase(paths) => {
            let message = format!(
                "cannot land: {head} conflicts with {MAIN_REF}, which is unchanged (request {}):{}\n\
                 bring it onto {MAIN_REF} with `scn update`, then land again",
                cr.id,
                path_list(paths)
            );
            return Err(Failure::new("needs_rebase", message)
                .with_paths(paths.clone())
                .into());
        }
        RequestStatus::Rejected(paths) if !paths.is_empty() => {
            let message = format!(
                "cannot land: you may not change these paths of {head} (request {}):{}\n\
                 a repository admin can grant you access to them",
                cr.id,
                path_list(paths)
            );
            return Err(Failure::new("permission_denied", message)
                .with_paths(paths.clone())
                .into());
        }
        RequestStatus::Rejected(_) => {
            if out.json {
                out.emit(&json!({"request": request_json(&cr), "head": head.to_hex()}))?;
                return Ok(());
            }
            writeln!(
                out,
                "nothing to land: {MAIN_REF} already has the changes of {head}"
            )?;
            return Ok(());
        }
        RequestStatus::Pending | RequestStatus::Landing => unreachable!("finished above"),
    };
    let text = !out.json;
    if landed == head {
        if text {
            writeln!(out, "landed {head} on {MAIN_REF}")?;
        }
    } else {
        if text {
            writeln!(out, "landed {head} on {MAIN_REF} as {landed}")?;
        }
        // The working copy follows the rebased copy, so the next commit
        // builds on what landed.
        match scion_client::switch(&s.ws, &s.remote, landed, false).await {
            Ok(()) => {}
            Err(SwitchError::Dirty(_)) => {
                if text {
                    writeln!(
                        out,
                        "the working copy stays at {head}: it has local changes; \
                         commit them and run `scn update`"
                    )?;
                } else {
                    out.emit(&json!({"request": request_json(&cr), "head": head.to_hex()}))?;
                }
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        s.state = s.ws.read_state()?;
    }
    s.state.base = landed;
    s.ws.write_state(&s.state)?;
    if !text {
        let at = s.state.current_head.to_hex();
        out.emit(&json!({"request": request_json(&cr), "head": at}))?;
    }
    Ok(())
}

async fn land_status(out: &mut Out, id: u64) -> Result<()> {
    let mut s = Session::open().await?;
    let repo = s.config.repo.clone();
    let cr = s.remote.get_change_request(&repo, id, false).await?;
    if out.json {
        out.emit(&json!({"request": request_json(&cr)}))?;
        return Ok(());
    }
    writeln!(out, "{}", describe(&cr))?;
    Ok(())
}

/// How long ago, in the largest whole unit: `42s`, `5m`, `3h`, `12d`.
fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

async fn requests(out: &mut Out, status: Option<StatusArg>, limit: u32) -> Result<()> {
    let mut s = Session::open().await?;
    let repo = s.config.repo.clone();
    let listed = s
        .remote
        .list_change_requests(&repo, status.map(Into::into), limit)
        .await?;
    if out.json {
        let listed: Vec<Value> = listed.iter().map(request_json).collect();
        out.emit(&json!({"requests": listed}))?;
        return Ok(());
    }
    if listed.is_empty() {
        writeln!(out, "no change requests")?;
        return Ok(());
    }
    let now = SystemTime::now();
    let short = |cs: ChangesetId| cs.to_string()[..SHORT_ID].to_owned();
    let width = listed.iter().map(|cr| cr.id.to_string().len()).max();
    let width = width.unwrap_or(0).max("ID".len());
    writeln!(
        out,
        "{:>width$}  {:<12}  {:<SHORT_ID$}  {:<SHORT_ID$}  AGE",
        "ID", "STATUS", "HEAD", "LANDED"
    )?;
    for cr in &listed {
        let (status, landed) = match &cr.status {
            RequestStatus::Pending => ("pending", None),
            RequestStatus::Landing => ("landing", None),
            RequestStatus::Landed(cs) => ("landed", Some(*cs)),
            RequestStatus::NeedsRebase(_) => ("needs-rebase", None),
            RequestStatus::Rejected(_) => ("rejected", None),
        };
        let landed = landed.map_or_else(|| "-".to_owned(), short);
        let age = format_age(now.duration_since(cr.submitted_at).unwrap_or_default());
        writeln!(
            out,
            "{:>width$}  {status:<12}  {}  {landed:<SHORT_ID$}  {age}",
            cr.id,
            short(cr.head)
        )?;
    }
    Ok(())
}

async fn update(out: &mut Out, rev: &str, markers: bool) -> Result<()> {
    let mut s = Session::open().await?;
    let onto = s.resolve(rev).await?;
    let old = s.state.current_head;
    let report = |head: ChangesetId, rebased: u32, conflicts: &[RepoPath]| {
        json!({
            "previous": old.to_hex(),
            "onto": onto.to_hex(),
            "head": head.to_hex(),
            "rebased": rebased,
            "conflicts": paths_json(conflicts),
        })
    };
    let done = if markers {
        match scion_client::update_with_markers(&s.ws, &s.remote, onto).await? {
            Marked::Updated(done) => done,
            Marked::Conflicted { conflicts } if out.json => {
                out.emit(&report(onto, 0, &conflicts))?;
                return Ok(());
            }
            Marked::Conflicted { conflicts } => {
                writeln!(
                    out,
                    "moved to {onto} ({rev}) with the changes of {old} as local edits; \
                     conflicts in:{}\nresolve them, then `scn commit` and `scn land`",
                    path_list(&conflicts)
                )?;
                return Ok(());
            }
        }
    } else {
        match scion_client::update(&s.ws, &s.remote, onto).await {
            Err(e @ SwitchError::Conflict { .. }) => {
                let message =
                    format!("{e}\nto resolve with conflict markers: scn update --conflicts {rev}");
                let SwitchError::Conflict { paths, .. } = e else {
                    unreachable!("matched above")
                };
                return Err(Failure::new("conflict", message).with_paths(paths).into());
            }
            done => done?,
        }
    };
    if out.json {
        out.emit(&report(done.head, done.created, &[]))?;
    } else if done.head == old {
        writeln!(out, "{old} already contains {rev}")?;
    } else {
        writeln!(
            out,
            "updated {old} to {} on {rev} ({} changesets rebased)",
            done.head, done.created
        )?;
    }
    Ok(())
}

async fn merge(out: &mut Out, rev: &str, message: Option<&str>, author: &str) -> Result<()> {
    let author = parse_identity(author)?;
    let date = now()?;
    let mut s = Session::open().await?;
    let theirs = s.resolve(rev).await?;
    let old = s.state.current_head;
    let message = message.map_or_else(|| format!("Merge {rev}"), str::to_owned);
    let head = scion_client::merge(&s.ws, &s.remote, theirs, (&author, date), &message).await?;
    if out.json {
        out.emit(&json!({
            "previous": old.to_hex(),
            "merged": theirs.to_hex(),
            "head": head.to_hex(),
        }))?;
    } else if head == old {
        writeln!(out, "{old} already contains {rev}")?;
    } else {
        writeln!(out, "merged {rev} into {old} as {head}")?;
    }
    Ok(())
}

async fn revert(out: &mut Out, rev: &str, message: Option<&str>, author: &str) -> Result<()> {
    let author = parse_identity(author)?;
    let date = now()?;
    let mut s = Session::open().await?;
    let target = s.resolve(rev).await?;
    let message = if let Some(m) = message {
        m.to_owned()
    } else {
        let repo = s.config.repo.clone();
        let [cs] = <[Changeset; 1]>::try_from(s.remote.log(&repo, target, 1).await?)
            .map_err(|_| format!("no changeset {target}"))?;
        let subject = cs.message.lines().next().unwrap_or_default();
        format!("Revert \"{subject}\"\n\nThis reverts {target}.\n")
    };
    let head = scion_client::revert(&s.ws, &s.remote, target, (&author, date), &message).await?;
    if out.json {
        out.emit(&json!({"reverted": target.to_hex(), "head": head.to_hex()}))?;
        return Ok(());
    }
    writeln!(out, "reverted {target} as {head}")?;
    Ok(())
}

async fn checkout(out: &mut Out, rev: &str, force: bool) -> Result<()> {
    let mut s = Session::open().await?;
    let to = s.resolve(rev).await?;
    scion_client::switch(&s.ws, &s.remote, to, force).await?;
    if out.json {
        out.emit(&json!({"head": to.to_hex()}))?;
        return Ok(());
    }
    writeln!(out, "checked out {to}")?;
    Ok(())
}

/// Most changesets `explain-conflict` lists per path and side.
const EXPLAIN_LIMIT: u32 = 10;

/// The changesets of `head`'s first-parent history that changed `path`
/// after `base` did, newest first; at most [`EXPLAIN_LIMIT`].
async fn changed_since(
    s: &mut Session,
    head: ChangesetId,
    path: &RepoPath,
    base_change: Option<ChangesetId>,
) -> Result<Vec<Changeset>> {
    let repo = s.config.repo.clone();
    let mut listed = s.remote.path_log(&repo, head, path, EXPLAIN_LIMIT).await?;
    if let Some(i) = listed.iter().position(|cs| Some(cs.id()) == base_change) {
        listed.truncate(i);
    }
    Ok(listed)
}

async fn explain_conflict(out: &mut Out, rev: &str) -> Result<()> {
    let mut s = Session::open().await?;
    let onto = s.resolve(rev).await?;
    let head = s.state.current_head;
    let repo = s.config.repo.clone();
    // The server's own rebase says what conflicts; a clean one only adds
    // changesets nothing points to.
    let (changeset, paths) = match s.remote.rebase(&repo, head, onto).await? {
        Rebased::Clean { .. } => (None, Vec::new()),
        Rebased::Conflict { changeset, paths } => (Some(changeset), paths),
    };
    let base = s.remote.merge_base(&repo, head, onto).await?;
    let mut conflicts = Vec::new();
    for path in paths {
        // The last change to the path at the merge base bounds both sides.
        let base_change = match base {
            Some(b) => s
                .remote
                .path_log(&repo, b, &path, 1)
                .await?
                .first()
                .map(Changeset::id),
            None => None,
        };
        let ours = changed_since(&mut s, head, &path, base_change).await?;
        let theirs = changed_since(&mut s, onto, &path, base_change).await?;
        conflicts.push((path, ours, theirs));
    }
    let subject = |cs: &Changeset| cs.message.lines().next().unwrap_or_default().to_owned();
    if out.json {
        let side = |list: &[Changeset]| -> Vec<Value> {
            list.iter()
                .map(|cs| {
                    json!({
                        "id": cs.id().to_hex(),
                        "subject": subject(cs),
                        "author": {"name": cs.author.name, "email": cs.author.email},
                    })
                })
                .collect()
        };
        let conflicts: Vec<Value> = conflicts
            .iter()
            .map(|(path, ours, theirs)| {
                json!({"path": path.as_str(), "ours": side(ours), "theirs": side(theirs)})
            })
            .collect();
        out.emit(&json!({
            "head": head.to_hex(),
            "onto": onto.to_hex(),
            "base": base.map(|b| b.to_hex()),
            "changeset": changeset.map(|c| c.to_hex()),
            "conflicts": conflicts,
        }))?;
        return Ok(());
    }
    let Some(changeset) = changeset else {
        writeln!(out, "{head} rebases cleanly onto {rev} ({onto})")?;
        return Ok(());
    };
    let base = base.map_or_else(|| "none".to_owned(), |b| b.to_string());
    writeln!(
        out,
        "{head} conflicts with {rev} ({onto}) when replaying {changeset}; merge base {base}"
    )?;
    for (path, ours, theirs) in &conflicts {
        writeln!(out, "{path}")?;
        for (side, list) in [("ours", ours), ("theirs", theirs)] {
            for cs in list {
                let short = &cs.id().to_hex()[..SHORT_ID];
                writeln!(out, "  {side:<6} {short} {}", subject(cs))?;
            }
        }
    }
    writeln!(
        out,
        "resolve with conflict markers: scn update --conflicts {rev}"
    )?;
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    let mut out = Out {
        w: io::BufWriter::new(io::stdout().lock()),
        json: cli.json,
    };
    let out = &mut out;
    match cli.command {
        Command::Login { server } => login(out, &server).await?,
        Command::Whoami { server } => whoami(out, server.as_deref()).await?,
        Command::Access { action } => access(out, action).await?,
        Command::Init { server, repo } => init(out, &server, &repo).await?,
        Command::Clone {
            server,
            repo,
            dest,
            sparse,
        } => clone(out, &server, &repo, dest, &sparse).await?,
        Command::Status => print_status(out)?,
        Command::Log {
            path,
            rev,
            limit,
            landed,
        } => log(out, path.as_deref(), &rev, limit, landed).await?,
        Command::Diff { from, to } => match (from, to) {
            (Some(from), Some(to)) => diff_revisions(out, &from, &to).await?,
            _ => diff_working_copy(out).await?,
        },
        Command::Blame { path, rev } => blame(out, &path, &rev).await?,
        Command::Add { paths } => add(out, &paths)?,
        Command::Commit {
            message,
            author,
            land,
        } => commit(out, &message, &author, land).await?,
        Command::Land {
            status: Some(id), ..
        } => land_status(out, id).await?,
        Command::Land { no_wait, .. } => land(out, no_wait).await?,
        Command::Requests { status, limit } => requests(out, status, limit).await?,
        Command::Update { rev, conflicts } => update(out, &rev, conflicts).await?,
        Command::Merge {
            rev,
            message,
            author,
        } => merge(out, &rev, message.as_deref(), &author).await?,
        Command::Revert {
            rev,
            message,
            author,
        } => revert(out, &rev, message.as_deref(), &author).await?,
        Command::Checkout { rev, force } => checkout(out, &rev, force).await?,
        Command::ExplainConflict { rev } => explain_conflict(out, &rev).await?,
    }
    out.flush()?;
    Ok(())
}

/// Report a failure: `error.json` on stdout with `--json`, else a line on
/// stderr.
fn report_error(json: bool, code: &str, message: &str, paths: &[RepoPath]) {
    if json {
        let v = json!({"error": {"code": code, "message": message, "paths": paths_json(paths)}});
        println!("{v}");
    } else {
        eprintln!("scn: {message}");
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if e.use_stderr() && std::env::args_os().any(|a| a == "--json") => {
            report_error(true, "usage", e.to_string().trim_end(), &[]);
            return ExitCode::from(2);
        }
        Err(e) => e.exit(),
    };
    let json = cli.json;
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let (code, paths) = classify(&*e);
            report_error(json, code, &e.to_string(), &paths);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_ages_in_the_largest_whole_unit() {
        for (secs, want) in [
            (0, "0s"),
            (59, "59s"),
            (60, "1m"),
            (3599, "59m"),
            (3600, "1h"),
            (86_399, "23h"),
            (86_400, "1d"),
            (40 * 86_400, "40d"),
        ] {
            assert_eq!(format_age(Duration::from_secs(secs)), want);
        }
    }

    #[test]
    fn parses_identities() {
        assert_eq!(
            parse_identity("Ada Lovelace <ada@example.com>").unwrap(),
            Identity::new("Ada Lovelace", "ada@example.com")
        );
        for bad in ["ada", "<ada@example.com>", "Ada <>", "Ada <ada"] {
            assert!(parse_identity(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
