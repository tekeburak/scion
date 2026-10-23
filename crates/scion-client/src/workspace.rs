//! The `.scn/` directory of a working copy.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use scion_types::{ChangesetId, RepoPath};
use serde::{Deserialize, Serialize};

use crate::index::{Index, mtime_ns};
use crate::sparse::{SparseError, SparseProfile};
use crate::status::clean_dirs;

/// Name of the metadata directory at the root of a working copy.
pub const SCN_DIR: &str = ".scn";

/// Failure reading or writing `.scn/`.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("{0}: {1}")]
    Io(PathBuf, #[source] io::Error),
    #[error("{0} is already a working copy")]
    AlreadyExists(PathBuf),
    #[error("not inside a working copy: {0}")]
    NotFound(PathBuf),
    #[error("{0}: {1}")]
    Parse(PathBuf, String),
    #[error(transparent)]
    Sparse(#[from] SparseError),
}

/// `.scn/config`: where the repository lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Server address, e.g. `http://scion.internal:7070`.
    pub server: String,
    /// Repository name on that server.
    pub repo: String,
}

/// A local label for a changeset this client cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub head: ChangesetId,
    /// RFC 3339 time of the last update, as written by the CLI.
    pub updated: String,
}

/// `.scn/state`: the anonymous head the working copy is based on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// Changeset the working copy is based on.
    pub current_head: ChangesetId,
    /// Last known merge-base with `main` (a cache).
    pub base: ChangesetId,
    /// Local labels; they never leave the machine.
    pub heads: BTreeMap<String, Head>,
}

/// On-disk form of [`State`]: ids as hex, as in `docs/DATA_MODEL.md`.
#[derive(Serialize, Deserialize)]
struct StateFile {
    current_head: String,
    base: String,
    #[serde(default)]
    heads: BTreeMap<String, HeadFile>,
}

#[derive(Serialize, Deserialize)]
struct HeadFile {
    head: String,
    updated: String,
}

/// A working copy: its root directory and the files under `.scn/`.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

fn read(path: PathBuf) -> Result<Vec<u8>, WorkspaceError> {
    std::fs::read(&path).map_err(|e| WorkspaceError::Io(path, e))
}

fn read_string(path: PathBuf) -> Result<String, WorkspaceError> {
    std::fs::read_to_string(&path).map_err(|e| WorkspaceError::Io(path, e))
}

/// The mtime of the file at `path`.
fn written_ns(path: &Path) -> Result<i64, WorkspaceError> {
    std::fs::metadata(path)
        .map(|meta| mtime_ns(&meta))
        .map_err(|e| WorkspaceError::Io(path.to_owned(), e))
}

/// Write `data` to a temporary file next to `path` and rename it into place,
/// so a crash never leaves a half-written state file.
fn write_atomic(path: PathBuf, data: &[u8]) -> Result<(), WorkspaceError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| WorkspaceError::Io(path, e))
}

fn parse_id(path: &Path, field: &str, s: &str) -> Result<ChangesetId, WorkspaceError> {
    s.parse()
        .map_err(|e| WorkspaceError::Parse(path.to_owned(), format!("{field}: {e}")))
}

impl Workspace {
    /// Create `.scn/` under `root` (creating `root` if needed) with `config`
    /// and a full sparse profile. Fails if `root` already is a working copy.
    pub fn init(root: &Path, config: &Config) -> Result<Self, WorkspaceError> {
        let dir = root.join(SCN_DIR);
        if dir.exists() {
            return Err(WorkspaceError::AlreadyExists(root.to_owned()));
        }
        std::fs::create_dir_all(&dir).map_err(|e| WorkspaceError::Io(dir.clone(), e))?;
        let ws = Self {
            root: root.to_owned(),
        };
        ws.write_config(config)?;
        ws.write_sparse(&SparseProfile::full())?;
        Ok(ws)
    }

    /// The working copy containing `path`: the nearest ancestor with `.scn/`.
    pub fn find(path: &Path) -> Result<Self, WorkspaceError> {
        let mut dir = Some(path);
        while let Some(d) = dir {
            if d.join(SCN_DIR).is_dir() {
                return Ok(Self { root: d.to_owned() });
            }
            dir = d.parent();
        }
        Err(WorkspaceError::NotFound(path.to_owned()))
    }

    /// Root directory of the working copy.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn file(&self, name: &str) -> PathBuf {
        self.root.join(SCN_DIR).join(name)
    }

    pub fn read_config(&self) -> Result<Config, WorkspaceError> {
        let path = self.file("config");
        let text = read_string(path.clone())?;
        toml::from_str(&text).map_err(|e| WorkspaceError::Parse(path, e.to_string()))
    }

    pub fn write_config(&self, config: &Config) -> Result<(), WorkspaceError> {
        let text = toml::to_string(config).expect("config serializes");
        write_atomic(self.file("config"), text.as_bytes())
    }

    pub fn read_state(&self) -> Result<State, WorkspaceError> {
        let path = self.file("state");
        let text = read_string(path.clone())?;
        let file: StateFile = toml::from_str(&text)
            .map_err(|e| WorkspaceError::Parse(path.clone(), e.to_string()))?;
        let mut heads = BTreeMap::new();
        for (label, h) in file.heads {
            let head = parse_id(&path, &format!("heads.{label}"), &h.head)?;
            heads.insert(
                label,
                Head {
                    head,
                    updated: h.updated,
                },
            );
        }
        Ok(State {
            current_head: parse_id(&path, "current_head", &file.current_head)?,
            base: parse_id(&path, "base", &file.base)?,
            heads,
        })
    }

    pub fn write_state(&self, state: &State) -> Result<(), WorkspaceError> {
        let file = StateFile {
            current_head: state.current_head.to_hex(),
            base: state.base.to_hex(),
            heads: state
                .heads
                .iter()
                .map(|(label, h)| {
                    (
                        label.clone(),
                        HeadFile {
                            head: h.head.to_hex(),
                            updated: h.updated.clone(),
                        },
                    )
                })
                .collect(),
        };
        let text = toml::to_string(&file).expect("state serializes");
        write_atomic(self.file("state"), text.as_bytes())
    }

    pub fn read_sparse(&self) -> Result<SparseProfile, WorkspaceError> {
        Ok(SparseProfile::parse(&read_string(self.file("sparse"))?)?)
    }

    pub fn write_sparse(&self, sparse: &SparseProfile) -> Result<(), WorkspaceError> {
        write_atomic(self.file("sparse"), sparse.render().as_bytes())
    }

    /// The index; empty before the first checkout.
    pub fn read_index(&self) -> Result<Index, WorkspaceError> {
        let path = self.file("index");
        if !path.exists() {
            return Ok(Index::default());
        }
        let bytes = read(path.clone())?;
        let mut index = Index::decode(&bytes)
            .map_err(|e| WorkspaceError::Parse(path.clone(), e.to_string()))?;
        index.written_ns = written_ns(&path)?;
        Ok(index)
    }

    /// Write `index`, first recording which of its directories hold nothing
    /// else.
    pub fn write_index(&self, index: &mut Index) -> Result<(), WorkspaceError> {
        index.dirs = clean_dirs(&self.root, index);
        let path = self.file("index");
        write_atomic(path.clone(), &index.encode())?;
        index.written_ns = written_ns(&path)?;
        Ok(())
    }

    /// A list of paths, one per line, from `.scn/<name>`; empty if absent.
    fn read_paths(&self, name: &str) -> Result<BTreeSet<RepoPath>, WorkspaceError> {
        let path = self.file(name);
        if !path.exists() {
            return Ok(BTreeSet::new());
        }
        read_string(path.clone())?
            .lines()
            .map(|l| {
                RepoPath::new(l).map_err(|e| WorkspaceError::Parse(path.clone(), e.to_string()))
            })
            .collect()
    }

    fn write_paths(&self, name: &str, paths: &BTreeSet<RepoPath>) -> Result<(), WorkspaceError> {
        let mut text = String::new();
        for p in paths {
            text.push_str(p.as_str());
            text.push('\n');
        }
        write_atomic(self.file(name), text.as_bytes())
    }

    /// Untracked paths marked for the next commit; empty if none.
    pub fn read_added(&self) -> Result<BTreeSet<RepoPath>, WorkspaceError> {
        self.read_paths("added")
    }

    pub fn write_added(&self, added: &BTreeSet<RepoPath>) -> Result<(), WorkspaceError> {
        self.write_paths("added", added)
    }

    /// Paths `update --conflicts` wrote conflict markers into, until the
    /// next commit; empty if none.
    pub fn read_conflicts(&self) -> Result<BTreeSet<RepoPath>, WorkspaceError> {
        self.read_paths("conflicts")
    }

    pub fn write_conflicts(&self, paths: &BTreeSet<RepoPath>) -> Result<(), WorkspaceError> {
        self.write_paths("conflicts", paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            server: "http://127.0.0.1:7070".into(),
            repo: "mono".into(),
        };
        let ws = Workspace::init(dir.path(), &config).unwrap();
        assert!(matches!(
            Workspace::init(dir.path(), &config),
            Err(WorkspaceError::AlreadyExists(_))
        ));
        assert_eq!(ws.read_config().unwrap(), config);
        assert_eq!(ws.read_sparse().unwrap(), SparseProfile::full());
        assert_eq!(ws.read_index().unwrap(), Index::default());
        assert!(ws.read_added().unwrap().is_empty());
        let added = BTreeSet::from([
            RepoPath::new("a/new.rs").unwrap(),
            RepoPath::new("b").unwrap(),
        ]);
        ws.write_added(&added).unwrap();
        assert_eq!(ws.read_added().unwrap(), added);

        let id = ChangesetId::hash(b"head");
        let state = State {
            current_head: id,
            base: ChangesetId::hash(b"base"),
            heads: BTreeMap::from([(
                "fix-cache-tier".to_owned(),
                Head {
                    head: id,
                    updated: "2026-10-14T09:12:00Z".into(),
                },
            )]),
        };
        ws.write_state(&state).unwrap();
        assert_eq!(ws.read_state().unwrap(), state);
        let text = std::fs::read_to_string(dir.path().join(".scn/state")).unwrap();
        assert!(text.contains(&format!("current_head = \"{id}\"")), "{text}");

        let sparse = SparseProfile::new(vec![RepoPath::new("srv").unwrap()]);
        ws.write_sparse(&sparse).unwrap();
        assert_eq!(ws.read_sparse().unwrap(), sparse);

        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(Workspace::find(&nested).unwrap().root(), dir.path());
        let outside = tempfile::tempdir().unwrap();
        assert!(Workspace::find(outside.path()).is_err());
    }
}
