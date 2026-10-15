//! `scn`: the Scion command-line client.
//!
//! Reading: `clone`, `status`, `log`, `diff`, `blame`. Every history question
//! is an RPC to `scion-server`; the working copy is only needed for `status`
//! and for diffing local edits.
//!
//! Writing: `add` marks untracked files for the next commit (there are no
//! ignore files); `commit` creates a changeset on the server and makes it the
//! working copy's anonymous head; `land` fast-forwards `main` to that head.
//! There is no branch command: work is shared by changeset id.
//!
//! A revision is `@` (the working copy's current head), a 64-hex changeset
//! id, a local head label, or a server ref (`main`, `release/*`).

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use scion_client::{
    Config, FileStatus, MAIN_REF, Remote, RemoteError, SparseProfile, State, Workspace,
    read_working_file, status,
};
use scion_types::{Changeset, ChangesetId, FileChange, FileType, Identity, RepoPath, Timestamp};

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

/// Hex digits of a changeset id shown where a full id would not fit.
const SHORT_ID: usize = 12;

#[derive(Parser)]
#[command(name = "scn", version, about = "Scion source control client")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a working copy of a repository at its current `main`.
    Clone {
        /// Server address, e.g. `http://127.0.0.1:7070`.
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
    },
    /// Fast-forward `main` to the current head.
    Land,
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

/// `YYYY-MM-DD HH:MM:SS +HHMM` in the author's own offset.
fn format_date(t: Timestamp) -> String {
    let offset = i64::from(t.tz_offset_minutes);
    let local = t.unix_secs + offset * 60;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let secs = local.rem_euclid(86_400);
    let sign = if offset < 0 { '-' } else { '+' };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} {sign}{:02}{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        offset.abs() / 60,
        offset.abs() % 60,
    )
}

fn kind_name(kind: FileType) -> &'static str {
    match kind {
        FileType::Regular => "regular",
        FileType::Executable => "executable",
        FileType::Symlink => "symlink",
    }
}

fn print_changeset(out: &mut impl Write, cs: &Changeset) -> io::Result<()> {
    writeln!(out, "changeset {}", cs.id())?;
    if cs.parents.len() > 1 {
        let parents: Vec<String> = cs.parents.iter().map(ChangesetId::to_hex).collect();
        writeln!(out, "Parents: {}", parents.join(" "))?;
    }
    writeln!(out, "Author: {} <{}>", cs.author.name, cs.author.email)?;
    writeln!(out, "Date:   {}", format_date(cs.author_date))?;
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
    let old_bytes = old.as_ref().map_or(&[][..], |(_, d)| d.as_slice());
    let new_bytes = new.as_ref().map_or(&[][..], |(_, d)| d.as_slice());
    if old_bytes == new_bytes {
        return Ok(());
    }
    let name = |side: &Side, prefix: &str| -> Cow<'static, str> {
        match side {
            Some(_) => format!("{prefix}/{path}").into(),
            None => "/dev/null".into(),
        }
    };
    let unified = diffy::DiffOptions::new()
        .set_original_filename(name(old, "a"))
        .set_modified_filename(name(new, "b"))
        .create_patch_bytes(old_bytes, new_bytes);
    out.write_all(&unified.to_bytes())
}

async fn clone(
    out: &mut impl Write,
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
    let ws = scion_client::clone(server, repo, &dest, SparseProfile::new(paths)).await?;
    let head = ws.read_state()?.current_head;
    let files = ws.read_index()?.entries.len();
    writeln!(
        out,
        "cloned {repo} at {head} into {} ({files} files)",
        dest.display()
    )?;
    Ok(())
}

fn print_status(out: &mut impl Write) -> Result<()> {
    let ws = Workspace::find(&std::env::current_dir()?)?;
    for (path, st) in status(ws.root(), &ws.read_index()?, &ws.read_added()?)? {
        writeln!(out, "{} {path}", st.code())?;
    }
    Ok(())
}

async fn log(out: &mut impl Write, path: Option<&str>, rev: &str, limit: u32) -> Result<()> {
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
    for cs in &changesets {
        print_changeset(out, cs)?;
    }
    Ok(())
}

/// Local edits against the checked-out head, as the next commit would record
/// them; untracked files are left out.
async fn diff_working_copy(out: &mut impl Write) -> Result<()> {
    let mut s = Session::open().await?;
    let index = s.ws.read_index()?;
    for (path, st) in status(s.ws.root(), &index, &s.ws.read_added()?)? {
        let old = match st {
            FileStatus::Untracked => continue,
            FileStatus::Added => None,
            FileStatus::Modified | FileStatus::Deleted => {
                let e = index.entries[&path];
                Some((e.kind, s.remote.get_blob(e.id).await?))
            }
        };
        let new = match st {
            FileStatus::Modified | FileStatus::Added => {
                Some(read_working_file(&s.ws.root().join(path.as_str()))?)
            }
            _ => None,
        };
        print_file_diff(out, &path, &old, &new)?;
    }
    Ok(())
}

async fn diff_revisions(out: &mut impl Write, from: &str, to: &str) -> Result<()> {
    let mut s = Session::open().await?;
    let old = s.resolve(from).await?;
    let new = s.resolve(to).await?;
    let repo = s.config.repo.clone();
    for e in s.remote.diff(&repo, Some(old), new).await? {
        let old = match e.old {
            Some(f) => Some((f.kind, s.remote.get_blob(f.id).await?)),
            None => None,
        };
        let new = match e.new {
            Some(f) => Some((f.kind, s.remote.get_blob(f.id).await?)),
            None => None,
        };
        print_file_diff(out, &e.path, &old, &new)?;
    }
    Ok(())
}

async fn blame(out: &mut impl Write, path: &str, rev: &str) -> Result<()> {
    let mut s = Session::open().await?;
    let cs = s.resolve(rev).await?;
    let path = s.repo_path(path)?;
    let repo = s.config.repo.clone();
    let runs = s.remote.blame(&repo, cs, &path).await?;
    // The newest changeset that touched the file holds its content.
    let last = s.remote.path_log(&repo, cs, &path, 1).await?;
    let Some(FileChange::Tracked { content, .. }) =
        last.first().and_then(|c| c.file_changes.get(&path))
    else {
        return Err(format!("server history has no content for {path}").into());
    };
    let data = s.remote.get_blob(*content).await?;
    let mut lines = data.split_inclusive(|b| *b == b'\n');
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

fn add(out: &mut impl Write, args: &[String]) -> Result<()> {
    let ws = Workspace::find(&std::env::current_dir()?)?;
    let mut added = ws.read_added()?;
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
                writeln!(out, "A {path}")?;
            }
        }
    }
    ws.write_added(&added)?;
    Ok(())
}

async fn commit(out: &mut impl Write, message: &str, author: &str) -> Result<()> {
    let author = parse_identity(author)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let date = Timestamp::new(i64::try_from(now)?, 0);
    let s = Session::open().await?;
    let id = scion_client::commit(&s.ws, &s.remote, author, date, message).await?;
    writeln!(out, "committed {id}")?;
    Ok(())
}

async fn land(out: &mut impl Write) -> Result<()> {
    let mut s = Session::open().await?;
    let repo = s.config.repo.clone();
    let head = s.state.current_head;
    let main = s.remote.get_ref(&repo, MAIN_REF).await?;
    if main.target == head {
        writeln!(out, "{MAIN_REF} is already at {head}")?;
        return Ok(());
    }
    let version = match s.remote.move_ref(&repo, MAIN_REF, head, main.version).await {
        Ok(v) => v,
        Err(RemoteError::Rpc(st)) if st.code() == tonic::Code::FailedPrecondition => {
            return Err(format!("cannot land: {}", st.message()).into());
        }
        Err(e) => return Err(e.into()),
    };
    s.state.base = head;
    s.ws.write_state(&s.state)?;
    writeln!(out, "landed {head} on {MAIN_REF} (version {version})")?;
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    match cli.command {
        Command::Clone {
            server,
            repo,
            dest,
            sparse,
        } => clone(&mut out, &server, &repo, dest, &sparse).await?,
        Command::Status => print_status(&mut out)?,
        Command::Log { path, rev, limit } => log(&mut out, path.as_deref(), &rev, limit).await?,
        Command::Diff { from, to } => match (from, to) {
            (Some(from), Some(to)) => diff_revisions(&mut out, &from, &to).await?,
            _ => diff_working_copy(&mut out).await?,
        },
        Command::Blame { path, rev } => blame(&mut out, &path, &rev).await?,
        Command::Add { paths } => add(&mut out, &paths)?,
        Command::Commit { message, author } => commit(&mut out, &message, &author).await?,
        Command::Land => land(&mut out).await?,
    }
    out.flush()?;
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("scn: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates_in_the_author_offset() {
        assert_eq!(
            format_date(Timestamp::new(0, 0)),
            "1970-01-01 00:00:00 +0000"
        );
        // 2026-10-14T09:12:00Z
        assert_eq!(
            format_date(Timestamp::new(1_791_969_120, 180)),
            "2026-10-14 12:12:00 +0300"
        );
        assert_eq!(
            format_date(Timestamp::new(951_782_400, -330)),
            "2000-02-28 18:30:00 -0530"
        );
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
