//! The connection to the server. In the MVP both planes go to `scion-server`
//! directly; the object plane moves to the nearest cache tier in Phase 4.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::login_service_client::LoginServiceClient;
use scion_proto::v1::object_service_client::ObjectServiceClient;
use scion_proto::v1::{
    BlameRequest, CommitAndLandRequest, CreateChangesetRequest, DiffRequest, FindLandingRequest,
    FinishLoginRequest, GetBlobRequest, GetChangeRequestRequest, GetObjectsRequest, GetRefRequest,
    GetTreeRequest, GrantRequest, HasBlobsRequest, InitRepoRequest, ListChangeRequestsRequest,
    ListGrantsRequest, LogRequest, LogResponse, MergeBaseRequest, MergeRequest, MoveRefRequest,
    PathLogRequest, PutBlobRequest, PutBlobsRequest, RebaseRequest, RevertRequest, RevokeRequest,
    StartLoginRequest, SubmitChangeRequestRequest, WhoAmIRequest, object,
};
use scion_types::{
    BlobId, Changeset, ChangesetId, Decode, Encode, FileType, Identity, RepoPath, Timestamp, Tree,
    TreeId,
};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

/// The status filter of [`Remote::list_change_requests`].
pub use scion_proto::v1::ChangeRequestStatus;

/// A principal's role on a directory of a repository (an empty
/// `path_prefix` for all of it), as [`Remote::list_grants`] reports it.
pub use scion_proto::v1::AccessGrant;

/// Deadline the client attaches to every RPC.
pub const RPC_DEADLINE: Duration = Duration::from_secs(30);

/// Largest message the client sends or accepts, as the server's: a blob at
/// the per-blob limit and the rest of its request.
const MAX_MESSAGE_BYTES: usize = scion_types::MAX_BLOB_BYTES + (1 << 20);

/// Failure talking to the server.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("cannot connect to {0}: {cause}", cause = causes(.1))]
    Connect(String, #[source] tonic::transport::Error),
    #[error("cannot read the certificates in SSL_CERT_FILE ({0}): {1}")]
    CertFile(String, #[source] std::io::Error),
    #[error(transparent)]
    Rpc(#[from] tonic::Status),
    #[error("server sent a malformed {0}")]
    Malformed(&'static str),
    #[error("cannot read the token for {0}: {1}")]
    Token(String, #[source] std::io::Error),
    #[error("the token for {0} (from {1}) is not a token")]
    BadToken(String, String),
}

/// One object of a [`Remote::get_objects`] reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    Tree(TreeId, Tree),
    Blob(BlobId, Vec<u8>),
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
    /// Rebased onto `main`, it would land nothing (no paths), or it changes
    /// these paths, which its submitter may not change.
    Rejected(Vec<RepoPath>),
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

/// Who a token names, as [`Remote::who_am_i`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhoAmI {
    pub principal: String,
    pub expires_at: SystemTime,
    /// The principal's grants in the repository asked about, by path prefix.
    pub grants: Vec<AccessGrant>,
}

/// A started GitHub device-flow login: the user enters `user_code` at
/// `verification_uri`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: Duration,
    pub interval: Duration,
}

/// Where a login is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginPoll {
    /// Not authorized yet; ask again after `interval`.
    Pending { interval: Duration },
    /// A token for `principal`, valid until `expires_at`.
    Done {
        token: String,
        principal: String,
        expires_at: SystemTime,
    },
}

fn paths(paths: Vec<String>) -> Result<Vec<RepoPath>, RemoteError> {
    paths
        .into_iter()
        .map(|p| RepoPath::new(p).map_err(|_| RemoteError::Malformed("path")))
        .collect()
}

fn change_request(cr: scion_proto::v1::ChangeRequest) -> Result<RemoteChangeRequest, RemoteError> {
    let status = match ChangeRequestStatus::try_from(cr.status) {
        Ok(ChangeRequestStatus::Pending) => RequestStatus::Pending,
        Ok(ChangeRequestStatus::Landing) => RequestStatus::Landing,
        Ok(ChangeRequestStatus::Landed) => RequestStatus::Landed(changeset_id(
            cr.landed.as_deref().unwrap_or_default(),
            "landed changeset",
        )?),
        Ok(ChangeRequestStatus::NeedsRebase) => RequestStatus::NeedsRebase(paths(cr.conflicts)?),
        Ok(ChangeRequestStatus::Rejected) => RequestStatus::Rejected(paths(cr.conflicts)?),
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

/// Write `token` to [`token_path`] for `server`, readable by its owner only;
/// returns the path.
pub fn save_token(server: &str, token: &str) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let path = token_path(server).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "neither XDG_CONFIG_HOME nor HOME is set",
        )
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    // `mode` applies only to a new file.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    writeln!(file, "{token}")?;
    Ok(path)
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

/// TLS for an `https://` server: the webpki roots, and the certificates in
/// `SSL_CERT_FILE` when it is set (as OpenSSL and curl read it), which is how
/// a server behind a private certificate authority is trusted.
fn tls_config() -> Result<ClientTlsConfig, RemoteError> {
    let config = ClientTlsConfig::new().with_webpki_roots();
    match std::env::var_os("SSL_CERT_FILE").filter(|f| !f.is_empty()) {
        None => Ok(config),
        Some(file) => {
            let pem = std::fs::read(&file)
                .map_err(|e| RemoteError::CertFile(Path::new(&file).display().to_string(), e))?;
            Ok(config.ca_certificate(Certificate::from_pem(pem)))
        }
    }
}

/// `e` and its sources, joined with `: `. tonic's transport error says only
/// "transport error"; the reason (refused, a certificate for another name)
/// is in its sources.
fn causes(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut next = e.source();
    while let Some(e) = next {
        out.push_str(": ");
        out.push_str(&e.to_string());
        next = e.source();
    }
    out
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
    pub login: LoginServiceClient<Channel>,
}

impl Remote {
    /// Connect to `server` (e.g. `http://127.0.0.1:7420`). Every request
    /// carries the server's token (see [`token_path`]) if there is one.
    pub async fn connect(server: &str) -> Result<Self, RemoteError> {
        match token_for(server)? {
            None => Self::open(server, None).await,
            Some((token, from)) => Self::open(server, Some((&token, &from))).await,
        }
    }

    /// Connect to `server` with `token`, whatever `SCN_TOKEN` and the token
    /// files say.
    pub async fn with_token(server: &str, token: &str) -> Result<Self, RemoteError> {
        Self::open(server, Some((token, "the caller"))).await
    }

    /// Connect, sending `token` (and where it came from, for the error) if
    /// given.
    async fn open(server: &str, token: Option<(&str, &str)>) -> Result<Self, RemoteError> {
        let bearer = match token {
            None => None,
            Some((token, from)) => Some(
                format!("Bearer {token}")
                    .parse()
                    .map_err(|_| RemoteError::BadToken(server.to_owned(), from.to_owned()))?,
            ),
        };
        let mut endpoint = Endpoint::from_shared(server.to_owned())
            .map_err(|e| RemoteError::Connect(server.to_owned(), e))?;
        if server.starts_with("https://") {
            endpoint = endpoint
                .tls_config(tls_config()?)
                .map_err(|e| RemoteError::Connect(server.to_owned(), e))?;
        }
        let channel = endpoint
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
            objects: ObjectServiceClient::with_interceptor(channel.clone(), bearer)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
            login: LoginServiceClient::new(channel),
        })
    }

    /// Start a GitHub device-flow login on the server.
    pub async fn start_login(&mut self) -> Result<LoginStart, RemoteError> {
        let resp = self
            .login
            .start_login(StartLoginRequest {})
            .await?
            .into_inner();
        Ok(LoginStart {
            device_code: resp.device_code,
            user_code: resp.user_code,
            verification_uri: resp.verification_uri,
            expires_in: Duration::from_secs(resp.expires_in_seconds.into()),
            interval: Duration::from_secs(resp.interval_seconds.into()),
        })
    }

    /// Ask whether the login of `device_code` was authorized.
    pub async fn finish_login(&mut self, device_code: &str) -> Result<LoginPoll, RemoteError> {
        let resp = self
            .login
            .finish_login(FinishLoginRequest {
                device_code: device_code.to_owned(),
            })
            .await?
            .into_inner();
        if resp.token.is_empty() {
            return Ok(LoginPoll::Pending {
                interval: Duration::from_secs(resp.interval_seconds.into()),
            });
        }
        let expires = u64::try_from(resp.expires_unix_seconds)
            .map_err(|_| RemoteError::Malformed("token expiry"))?;
        Ok(LoginPoll::Done {
            token: resp.token,
            principal: resp.principal,
            expires_at: UNIX_EPOCH + Duration::from_secs(expires),
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

    /// Fetch one tree, for `repo`.
    pub async fn get_tree(&mut self, repo: &str, id: TreeId) -> Result<Tree, RemoteError> {
        let resp = self
            .objects
            .get_tree(GetTreeRequest {
                id: id.as_bytes().to_vec(),
                repo: repo.to_owned(),
            })
            .await?
            .into_inner();
        Tree::decode(&resp.tree).map_err(|_| RemoteError::Malformed("tree"))
    }

    /// Fetch one blob, for `repo`.
    pub async fn get_blob(&mut self, repo: &str, id: BlobId) -> Result<Vec<u8>, RemoteError> {
        Ok(self
            .objects
            .get_blob(GetBlobRequest {
                id: id.as_bytes().to_vec(),
                repo: repo.to_owned(),
            })
            .await?
            .into_inner()
            .data)
    }

    /// Fetch `trees`, `subtrees` with every tree beneath them, and `blobs`
    /// of `repo` in one call, at most [`scion_proto::MAX_IDS`] ids in all.
    /// Objects arrive in no particular order, except that a tree beneath
    /// one of `subtrees` comes after its parent.
    pub async fn get_objects(
        &mut self,
        repo: &str,
        trees: &[TreeId],
        subtrees: &[TreeId],
        blobs: &[BlobId],
    ) -> Result<impl futures_util::Stream<Item = Result<Fetched, RemoteError>> + use<>, RemoteError>
    {
        use futures_util::StreamExt;
        let raw = |ids: &[TreeId]| ids.iter().map(|id| id.as_bytes().to_vec()).collect();
        let resp = self
            .objects
            .get_objects(GetObjectsRequest {
                repo: repo.to_owned(),
                trees: raw(trees),
                subtrees: raw(subtrees),
                blobs: blobs.iter().map(|id| id.as_bytes().to_vec()).collect(),
            })
            .await?
            .into_inner();
        Ok(resp.map(|o| {
            let o = o?;
            match o.object {
                Some(object::Object::Tree(bytes)) => Ok(Fetched::Tree(
                    TreeId::try_from_slice(&o.id).map_err(|_| RemoteError::Malformed("tree id"))?,
                    Tree::decode(&bytes).map_err(|_| RemoteError::Malformed("tree"))?,
                )),
                Some(object::Object::Blob(data)) => Ok(Fetched::Blob(
                    BlobId::try_from_slice(&o.id).map_err(|_| RemoteError::Malformed("blob id"))?,
                    data,
                )),
                None => Err(RemoteError::Malformed("object")),
            }
        }))
    }

    /// Whether each of `ids` is stored, at most [`scion_proto::MAX_IDS`].
    pub async fn has_blobs(
        &mut self,
        repo: &str,
        ids: &[BlobId],
    ) -> Result<Vec<bool>, RemoteError> {
        let present = self
            .objects
            .has_blobs(HasBlobsRequest {
                repo: repo.to_owned(),
                ids: ids.iter().map(|id| id.as_bytes().to_vec()).collect(),
            })
            .await?
            .into_inner()
            .present;
        if present.len() != ids.len() {
            return Err(RemoteError::Malformed("blob presence"));
        }
        Ok(present)
    }

    /// Upload `blobs` for `repo` in one call.
    pub async fn put_blobs(&mut self, repo: &str, blobs: Vec<Vec<u8>>) -> Result<(), RemoteError> {
        let repo = repo.to_owned();
        let messages =
            futures_util::stream::iter(blobs.into_iter().map(move |data| PutBlobsRequest {
                repo: repo.clone(),
                data,
            }));
        self.objects.put_blobs(messages).await?;
        Ok(())
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

    /// Upload one blob for `repo`; returns its id.
    pub async fn put_blob(&mut self, repo: &str, data: Vec<u8>) -> Result<BlobId, RemoteError> {
        let resp = self
            .objects
            .put_blob(PutBlobRequest {
                data,
                repo: repo.to_owned(),
            })
            .await?
            .into_inner();
        BlobId::try_from_slice(&resp.id).map_err(|_| RemoteError::Malformed("blob id"))
    }

    /// Create repository `repo` with `main` at its root changeset, the
    /// caller its admin; returns the root. `ALREADY_EXISTS` if it exists.
    pub async fn init_repo(&mut self, repo: &str) -> Result<ChangesetId, RemoteError> {
        let resp = self
            .control
            .init_repo(InitRepoRequest {
                repo: repo.to_owned(),
            })
            .await?
            .into_inner();
        changeset_id(&resp.main, "main")
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

    /// Create `cs` and ask the server to land it on `main`, in one round
    /// trip; its blobs must be uploaded first. The reply comes as from
    /// [`submit_change_request`](Self::submit_change_request) with `wait`;
    /// the request's `head` is `cs`.
    pub async fn commit_and_land(
        &mut self,
        cs: &Changeset,
    ) -> Result<RemoteChangeRequest, RemoteError> {
        let resp = self
            .control
            .commit_and_land(CommitAndLandRequest {
                changeset: cs.encode_to_vec(),
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

    /// Give `principal` `role` on `path_prefix` (empty for the whole
    /// repository) in `repo`.
    pub async fn grant(
        &mut self,
        repo: &str,
        principal: &str,
        role: &str,
        path_prefix: &str,
    ) -> Result<(), RemoteError> {
        self.control
            .grant(GrantRequest {
                repo: repo.to_owned(),
                grant: Some(AccessGrant {
                    principal: principal.to_owned(),
                    path_prefix: path_prefix.to_owned(),
                    role: role.to_owned(),
                }),
            })
            .await?;
        Ok(())
    }

    /// Remove `principal`'s grant on `path_prefix` in `repo`.
    pub async fn revoke(
        &mut self,
        repo: &str,
        principal: &str,
        path_prefix: &str,
    ) -> Result<(), RemoteError> {
        self.control
            .revoke(RevokeRequest {
                repo: repo.to_owned(),
                principal: principal.to_owned(),
                path_prefix: path_prefix.to_owned(),
            })
            .await?;
        Ok(())
    }

    /// Every grant in `repo`, by principal then path prefix.
    pub async fn list_grants(&mut self, repo: &str) -> Result<Vec<AccessGrant>, RemoteError> {
        let resp = self
            .control
            .list_grants(ListGrantsRequest {
                repo: repo.to_owned(),
            })
            .await?
            .into_inner();
        Ok(resp.grants)
    }

    /// Who this connection's token names, and its grants in `repo` unless
    /// `repo` is empty.
    pub async fn who_am_i(&mut self, repo: &str) -> Result<WhoAmI, RemoteError> {
        let resp = self
            .control
            .who_am_i(WhoAmIRequest {
                repo: repo.to_owned(),
            })
            .await?
            .into_inner();
        let expires = u64::try_from(resp.expires_unix_seconds)
            .map_err(|_| RemoteError::Malformed("token expiry"))?;
        Ok(WhoAmI {
            principal: resp.principal,
            expires_at: UNIX_EPOCH + Duration::from_secs(expires),
            grants: resp.grants,
        })
    }
}
