//! The connection to the server. In the MVP both planes go to `scion-server`
//! directly; the object plane moves to the nearest cache tier in Phase 4.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::object_service_client::ObjectServiceClient;
use scion_proto::v1::{
    BlameRequest, CreateChangesetRequest, DiffRequest, FindLandingRequest, GetBlobRequest,
    GetChangeRequestRequest, GetRefRequest, GetTreeRequest, ListChangeRequestsRequest, LogRequest,
    LogResponse, MergeBaseRequest, MergeRequest, MoveRefRequest, PathLogRequest, PutBlobRequest,
    RebaseRequest, RevertRequest, SubmitChangeRequestRequest,
};
use scion_types::{
    BlobId, Changeset, ChangesetId, Decode, Encode, FileType, Identity, RepoPath, Timestamp, Tree,
    TreeId,
};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};

/// The status filter of [`Remote::list_change_requests`].
pub use scion_proto::v1::ChangeRequestStatus;

/// Deadline the client attaches to every RPC.
pub const RPC_DEADLINE: Duration = Duration::from_secs(30);

/// Largest message the client accepts; matches the server's per-blob limit.
const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Failure talking to the server.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("cannot connect to {0}: {1}")]
    Connect(String, #[source] tonic::transport::Error),
    #[error(transparent)]
    Rpc(#[from] tonic::Status),
    #[error("server sent a malformed {0}")]
    Malformed(&'static str),
    #[error("cannot read the token for {0}: {1}")]
    Token(String, #[source] std::io::Error),
    #[error("the token for {0} (from {1}) is not a token")]
    BadToken(String, String),
}

/// A ref as the server reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteRef {
    pub target: ChangesetId,
    pub version: u64,
    pub root_tree: TreeId,
}

/// One side of a [`DiffEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileEntry {
    pub id: BlobId,
    pub kind: FileType,
    pub size: u64,
}

/// A file that differs between two changesets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub path: RepoPath,
    /// `None` for an added file.
    pub old: Option<FileEntry>,
    /// `None` for a deleted file.
    pub new: Option<FileEntry>,
}

/// What the server's `Rebase` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebased {
    /// The rebased head and how many changesets were created for it.
    Clean { head: ChangesetId, created: u32 },
    /// Replaying `changeset` conflicts on `paths`.
    Conflict {
        changeset: ChangesetId,
        paths: Vec<RepoPath>,
    },
}

/// What the server's `Merge` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    /// The merge changeset; `created` is false when `ours` already
    /// contained `theirs` and `merged` is `ours`.
    Clean { merged: ChangesetId, created: bool },
    /// The heads conflict on `paths`; nothing was created.
    Conflict { paths: Vec<RepoPath> },
}

/// What the server's `Revert` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reverted {
    /// The changeset that undoes the reverted one.
    Clean { reverted: ChangesetId },
    /// These paths changed again since the reverted changeset; nothing was
    /// created.
    ChangedSince { paths: Vec<RepoPath> },
}

/// Consecutive lines last changed by one changeset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlameRun {
    pub changeset: ChangesetId,
    pub lines: u64,
}

/// Where a change request is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestStatus {
    /// Waiting for the land worker.
    Pending,
    /// Being rebased onto `main` and landed.
    Landing,
    /// On `main` as this changeset.
    Landed(ChangesetId),
    /// Conflicts with `main` at these paths; `main` is untouched.
    NeedsRebase(Vec<RepoPath>),
    /// Rebased onto `main`, it would land nothing.
    Rejected,
}

impl RequestStatus {
    /// Whether the land worker is done with the request.
    pub fn is_finished(&self) -> bool {
        !matches!(self, Self::Pending | Self::Landing)
    }
}

/// A change request as the server reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteChangeRequest {
    pub id: u64,
    pub head: ChangesetId,
    /// `main`'s changeset when the request was submitted.
    pub base: ChangesetId,
    pub status: RequestStatus,
    pub submitted_at: SystemTime,
    /// When `main` moved to the landed changeset; set once landed.
    pub landed_at: Option<SystemTime>,
}

fn change_request(cr: scion_proto::v1::ChangeRequest) -> Result<RemoteChangeRequest, RemoteError> {
    let status = match ChangeRequestStatus::try_from(cr.status) {
        Ok(ChangeRequestStatus::Pending) => RequestStatus::Pending,
        Ok(ChangeRequestStatus::Landing) => RequestStatus::Landing,
        Ok(ChangeRequestStatus::Landed) => RequestStatus::Landed(changeset_id(
            cr.landed.as_deref().unwrap_or_default(),
            "landed changeset",
        )?),
        Ok(ChangeRequestStatus::NeedsRebase) => RequestStatus::NeedsRebase(
            cr.conflicts
                .into_iter()
                .map(|p| RepoPath::new(p).map_err(|_| RemoteError::Malformed("path")))
                .collect::<Result<_, _>>()?,
        ),
        Ok(ChangeRequestStatus::Rejected) => RequestStatus::Rejected,
        Ok(ChangeRequestStatus::Unspecified) | Err(_) => {
            return Err(RemoteError::Malformed("change request status"));
        }
    };
    Ok(RemoteChangeRequest {
        id: cr.id,
        head: changeset_id(&cr.head, "change request head")?,
        base: changeset_id(&cr.base, "change request base")?,
        status,
        submitted_at: UNIX_EPOCH + Duration::from_micros(cr.submitted_unix_micros),
        landed_at: cr
            .landed_unix_micros
            .map(|us| UNIX_EPOCH + Duration::from_micros(us)),
    })
}

fn changeset_id(bytes: &[u8], what: &'static str) -> Result<ChangesetId, RemoteError> {
    ChangesetId::try_from_slice(bytes).map_err(|_| RemoteError::Malformed(what))
}

fn changesets(resp: &LogResponse) -> Result<Vec<Changeset>, RemoteError> {
    resp.changesets
        .iter()
        .map(|b| Changeset::decode(b).map_err(|_| RemoteError::Malformed("changeset")))
        .collect()
}

fn file_entry(f: &scion_proto::v1::FileEntry) -> Result<FileEntry, RemoteError> {
    Ok(FileEntry {
        id: BlobId::try_from_slice(&f.id).map_err(|_| RemoteError::Malformed("blob id"))?,
        kind: match scion_proto::v1::FileType::try_from(f.kind) {
            Ok(scion_proto::v1::FileType::Regular) => FileType::Regular,
            Ok(scion_proto::v1::FileType::Executable) => FileType::Executable,
            Ok(scion_proto::v1::FileType::Symlink) => FileType::Symlink,
            Err(_) => return Err(RemoteError::Malformed("file type")),
        },
        size: f.size,
    })
}

/// The file `scn login` writes the token for `server` to:
/// `$XDG_CONFIG_HOME/scn/tokens/<server>` (`~/.config` when unset), the
/// server address with every character but ASCII letters, digits, `.` and `-`
/// turned into `_`. `None` when neither variable is set.
pub fn token_path(server: &str) -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    let name: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(config.join("scn").join("tokens").join(name))
}

/// The token to send to `server`: `SCN_TOKEN` if set, else the contents of
/// [`token_path`], else none.
fn token_for(server: &str) -> Result<Option<(String, String)>, RemoteError> {
    if let Some(t) = std::env::var("SCN_TOKEN").ok().filter(|t| !t.is_empty()) {
        return Ok(Some((t, "SCN_TOKEN".to_owned())));
    }
    let Some(path) = token_path(server) else {
        return Ok(None);
    };
    match std::fs::read_to_string(&path) {
        Ok(t) => Ok(Some((t.trim().to_owned(), path.display().to_string()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(RemoteError::Token(server.to_owned(), e)),
    }
}

/// Adds `authorization: Bearer <token>` to every request, when there is a
/// token.
#[derive(Debug, Clone)]
pub struct Bearer(Option<MetadataValue<Ascii>>);

impl Interceptor for Bearer {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(v) = &self.0 {
            req.metadata_mut().insert("authorization", v.clone());
        }
        Ok(req)
    }
}

/// Clients for both planes of one server.
#[derive(Debug, Clone)]
pub struct Remote {
    pub control: ControlServiceClient<InterceptedService<Channel, Bearer>>,
    pub objects: ObjectServiceClient<InterceptedService<Channel, Bearer>>,
}

impl Remote {
    /// Connect to `server` (e.g. `http://127.0.0.1:7070`). Every request
    /// carries the server's token (see [`token_path`]) if there is one.
    pub async fn connect(server: &str) -> Result<Self, RemoteError> {
        let bearer = match token_for(server)? {
            None => None,
            Some((token, from)) => Some(
                format!("Bearer {token}")
                    .parse()
                    .map_err(|_| RemoteError::BadToken(server.to_owned(), from))?,
            ),
        };
        let channel = Endpoint::from_shared(server.to_owned())
            .map_err(|e| RemoteError::Connect(server.to_owned(), e))?
            .timeout(RPC_DEADLINE)
            .connect_timeout(RPC_DEADLINE)
            .connect()
            .await
            .map_err(|e| RemoteError::Connect(server.to_owned(), e))?;
        let bearer = Bearer(bearer);
        Ok(Self {
            control: ControlServiceClient::with_interceptor(channel.clone(), bearer.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
            objects: ObjectServiceClient::with_interceptor(channel, bearer)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        })
    }

    /// Read a server ref.
    pub async fn get_ref(&mut self, repo: &str, name: &str) -> Result<RemoteRef, RemoteError> {
        let resp = self
            .control
            .get_ref(GetRefRequest {
                repo: repo.to_owned(),
                name: name.to_owned(),
            })
            .await?
            .into_inner();
        Ok(RemoteRef {
            target: changeset_id(&resp.target, "ref target")?,
            version: resp.version,
            root_tree: TreeId::try_from_slice(&resp.root_tree)
                .map_err(|_| RemoteError::Malformed("root tree id"))?,
        })
    }

    /// Fetch one tree.
    pub async fn get_tree(&mut self, id: TreeId) -> Result<Tree, RemoteError> {
        let resp = self
            .objects
            .get_tree(GetTreeRequest {
                id: id.as_bytes().to_vec(),
            })
            .await?
            .into_inner();
        Tree::decode(&resp.tree).map_err(|_| RemoteError::Malformed("tree"))
    }

    /// Fetch one blob.
    pub async fn get_blob(&mut self, id: BlobId) -> Result<Vec<u8>, RemoteError> {
        Ok(self
            .objects
            .get_blob(GetBlobRequest {
                id: id.as_bytes().to_vec(),
            })
            .await?
            .into_inner()
            .data)
    }

    /// First-parent history of `head`, newest first, at most `limit` long.
    pub async fn log(
        &mut self,
        repo: &str,
        head: ChangesetId,
        limit: u32,
    ) -> Result<Vec<Changeset>, RemoteError> {
        let resp = self
            .control
            .log(LogRequest {
                repo: repo.to_owned(),
                head: head.as_bytes().to_vec(),
                limit,
            })
            .await?
            .into_inner();
        changesets(&resp)
    }

    /// First-parent history of `head` that touched `path`, newest first.
    pub async fn path_log(
        &mut self,
        repo: &str,
        head: ChangesetId,
        path: &RepoPath,
        limit: u32,
    ) -> Result<Vec<Changeset>, RemoteError> {
        let resp = self
            .control
            .path_log(PathLogRequest {
                repo: repo.to_owned(),
                head: head.as_bytes().to_vec(),
                path: path.as_str().to_owned(),
                limit,
            })
            .await?
            .into_inner();
        changesets(&resp)
    }

    /// Files that differ between `old` (the empty tree if `None`) and `new`,
    /// sorted by path.
    pub async fn diff(
        &mut self,
        repo: &str,
        old: Option<ChangesetId>,
        new: ChangesetId,
    ) -> Result<Vec<DiffEntry>, RemoteError> {
        let resp = self
            .control
            .diff(DiffRequest {
                repo: repo.to_owned(),
                old: old.map(|id| id.as_bytes().to_vec()),
                new: new.as_bytes().to_vec(),
            })
            .await?
            .into_inner();
        resp.entries
            .into_iter()
            .map(|e| {
                Ok(DiffEntry {
                    path: RepoPath::new(e.path).map_err(|_| RemoteError::Malformed("path"))?,
                    old: e.old.as_ref().map(file_entry).transpose()?,
                    new: e.new.as_ref().map(file_entry).transpose()?,
                })
            })
            .collect()
    }

    /// Replay `head`'s changesets that `onto` does not contain on top of
    /// `onto`, on the server.
    pub async fn rebase(
        &mut self,
        repo: &str,
        head: ChangesetId,
        onto: ChangesetId,
    ) -> Result<Rebased, RemoteError> {
        let resp = self
            .control
            .rebase(RebaseRequest {
                repo: repo.to_owned(),
                head: head.as_bytes().to_vec(),
                onto: onto.as_bytes().to_vec(),
            })
            .await?
            .into_inner();
        if let Some(head) = resp.head {
            return Ok(Rebased::Clean {
                head: changeset_id(&head, "rebased head")?,
                created: resp.created,
            });
        }
        Ok(Rebased::Conflict {
            changeset: changeset_id(&resp.conflicted, "conflicted changeset")?,
            paths: resp
                .conflicts
                .into_iter()
                .map(|p| RepoPath::new(p).map_err(|_| RemoteError::Malformed("path")))
                .collect::<Result<_, _>>()?,
        })
    }

    /// The merge base of `a` and `b`; `None` if they share no ancestor.
    pub async fn merge_base(
        &mut self,
        repo: &str,
        a: ChangesetId,
        b: ChangesetId,
    ) -> Result<Option<ChangesetId>, RemoteError> {
        let resp = self
            .control
            .merge_base(MergeBaseRequest {
                repo: repo.to_owned(),
                a: a.as_bytes().to_vec(),
                b: b.as_bytes().to_vec(),
            })
            .await?
            .into_inner();
        resp.base
            .map(|base| changeset_id(&base, "merge base"))
            .transpose()
    }

    /// Merge `theirs` into `ours` on the server, creating a changeset with
    /// parents `[ours, theirs]` written by `author` at `date`.
    pub async fn merge(
        &mut self,
        repo: &str,
        (ours, theirs): (ChangesetId, ChangesetId),
        (author, date): (&Identity, Timestamp),
        message: &str,
    ) -> Result<Merged, RemoteError> {
        let resp = self
            .control
            .merge(MergeRequest {
                repo: repo.to_owned(),
                ours: ours.as_bytes().to_vec(),
                theirs: theirs.as_bytes().to_vec(),
                author_name: author.name.clone(),
                author_email: author.email.clone(),
                author_unix_secs: date.unix_secs,
                author_tz_offset_minutes: date.tz_offset_minutes,
                message: message.to_owned(),
            })
            .await?
            .into_inner();
        if let Some(merged) = resp.merged {
            return Ok(Merged::Clean {
                merged: changeset_id(&merged, "merge changeset")?,
                created: resp.created,
            });
        }
        Ok(Merged::Conflict {
            paths: resp
                .conflicts
                .into_iter()
                .map(|p| RepoPath::new(p).map_err(|_| RemoteError::Malformed("path")))
                .collect::<Result<_, _>>()?,
        })
    }

    /// Undo `target` on `head` on the server, creating a changeset on
    /// `head` written by `author` at `date`.
    pub async fn revert(
        &mut self,
        repo: &str,
        (head, target): (ChangesetId, ChangesetId),
        (author, date): (&Identity, Timestamp),
        message: &str,
    ) -> Result<Reverted, RemoteError> {
        let resp = self
            .control
            .revert(RevertRequest {
                repo: repo.to_owned(),
                head: head.as_bytes().to_vec(),
                changeset: target.as_bytes().to_vec(),
                author_name: author.name.clone(),
                author_email: author.email.clone(),
                author_unix_secs: date.unix_secs,
                author_tz_offset_minutes: date.tz_offset_minutes,
                message: message.to_owned(),
            })
            .await?
            .into_inner();
        if let Some(reverted) = resp.reverted {
            return Ok(Reverted::Clean {
                reverted: changeset_id(&reverted, "revert changeset")?,
            });
        }
        Ok(Reverted::ChangedSince {
            paths: resp
                .changed_since
                .into_iter()
                .map(|p| RepoPath::new(p).map_err(|_| RemoteError::Malformed("path")))
                .collect::<Result<_, _>>()?,
        })
    }

    /// Line attribution of the file at `path` in `changeset`.
    pub async fn blame(
        &mut self,
        repo: &str,
        changeset: ChangesetId,
        path: &RepoPath,
    ) -> Result<Vec<BlameRun>, RemoteError> {
        let resp = self
            .control
            .blame(BlameRequest {
                repo: repo.to_owned(),
                changeset: changeset.as_bytes().to_vec(),
                path: path.as_str().to_owned(),
            })
            .await?
            .into_inner();
        resp.runs
            .into_iter()
            .map(|r| {
                Ok(BlameRun {
                    changeset: changeset_id(&r.changeset, "blame changeset")?,
                    lines: r.lines,
                })
            })
            .collect()
    }

    /// Upload one blob; returns its id.
    pub async fn put_blob(&mut self, data: Vec<u8>) -> Result<BlobId, RemoteError> {
        let resp = self
            .objects
            .put_blob(PutBlobRequest { data })
            .await?
            .into_inner();
        BlobId::try_from_slice(&resp.id).map_err(|_| RemoteError::Malformed("blob id"))
    }

    /// Create `cs` on the server; its blobs must be uploaded first.
    pub async fn create_changeset(&mut self, cs: &Changeset) -> Result<ChangesetId, RemoteError> {
        let resp = self
            .control
            .create_changeset(CreateChangesetRequest {
                changeset: cs.encode_to_vec(),
            })
            .await?
            .into_inner();
        changeset_id(&resp.id, "changeset id")
    }

    /// Create server ref `name` at `target`; `ALREADY_EXISTS` if it exists.
    /// Returns its version.
    pub async fn create_ref(
        &mut self,
        repo: &str,
        name: &str,
        target: ChangesetId,
    ) -> Result<u64, RemoteError> {
        Ok(self
            .control
            .move_ref(MoveRefRequest {
                repo: repo.to_owned(),
                name: name.to_owned(),
                target: target.as_bytes().to_vec(),
                expected_version: None,
            })
            .await?
            .into_inner()
            .version)
    }

    /// Move server ref `name` to `target` if it is still at
    /// `expected_version` and `target` descends from it; returns the new
    /// version.
    pub async fn move_ref(
        &mut self,
        repo: &str,
        name: &str,
        target: ChangesetId,
        expected_version: u64,
    ) -> Result<u64, RemoteError> {
        Ok(self
            .control
            .move_ref(MoveRefRequest {
                repo: repo.to_owned(),
                name: name.to_owned(),
                target: target.as_bytes().to_vec(),
                expected_version: Some(expected_version),
            })
            .await?
            .into_inner()
            .version)
    }

    /// Ask the server to land `head` on `main`. With `wait`, the reply comes
    /// once the request is finished, or still open after the server's
    /// longest wait.
    pub async fn submit_change_request(
        &mut self,
        repo: &str,
        head: ChangesetId,
        wait: bool,
    ) -> Result<RemoteChangeRequest, RemoteError> {
        let resp = self
            .control
            .submit_change_request(SubmitChangeRequestRequest {
                repo: repo.to_owned(),
                head: head.as_bytes().to_vec(),
                wait,
            })
            .await?
            .into_inner();
        change_request(resp)
    }

    /// Read change request `id`; `wait` as in
    /// [`submit_change_request`](Self::submit_change_request).
    pub async fn get_change_request(
        &mut self,
        repo: &str,
        id: u64,
        wait: bool,
    ) -> Result<RemoteChangeRequest, RemoteError> {
        let resp = self
            .control
            .get_change_request(GetChangeRequestRequest {
                repo: repo.to_owned(),
                id,
                wait,
            })
            .await?
            .into_inner();
        change_request(resp)
    }

    /// The newest `limit` change requests of `repo`, only those in `status`
    /// if given, newest first.
    pub async fn list_change_requests(
        &mut self,
        repo: &str,
        status: Option<ChangeRequestStatus>,
        limit: u32,
    ) -> Result<Vec<RemoteChangeRequest>, RemoteError> {
        let resp = self
            .control
            .list_change_requests(ListChangeRequestsRequest {
                repo: repo.to_owned(),
                status: status.unwrap_or(ChangeRequestStatus::Unspecified).into(),
                limit,
            })
            .await?
            .into_inner();
        resp.requests.into_iter().map(change_request).collect()
    }

    /// The request that put each of `changesets` on `main`, in the order
    /// asked; a changeset no request landed is left out.
    pub async fn find_landings(
        &mut self,
        repo: &str,
        changesets: &[ChangesetId],
    ) -> Result<Vec<(ChangesetId, RemoteChangeRequest)>, RemoteError> {
        let resp = self
            .control
            .find_landing(FindLandingRequest {
                repo: repo.to_owned(),
                changesets: changesets.iter().map(|cs| cs.as_bytes().to_vec()).collect(),
            })
            .await?
            .into_inner();
        resp.landings
            .into_iter()
            .map(|l| {
                let request = l.request.ok_or(RemoteError::Malformed("landing"))?;
                Ok((
                    changeset_id(&l.changeset, "landed changeset")?,
                    change_request(request)?,
                ))
            })
            .collect()
    }
}
