//! The HTTP/JSON API under `/api/v1`: the control plane and the object plane
//! for clients that do not speak gRPC. Each route builds the RPC's request
//! and calls the same [`ControlServer`] or [`ObjectServer`] method the gRPC
//! services run, so authorization, validation and the land worker are
//! shared; this module only translates. Ids are 64 hex digits, blobs travel
//! as raw bytes, changesets and trees as JSON that the server encodes
//! canonically itself (never hashing the JSON).
//!
//! The caller's token comes in `Authorization: Bearer <token>`. A `GET`
//! without one reads as [`ANYONE`], and every read also holds what
//! [`ANYONE`] is granted, as on the web interface's pages; anything else
//! needs a token. `docs/API.md` lists every route.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use scion_derived::{Deriver, RootTree};
use scion_proto::v1::control_service_server::ControlService;
use scion_proto::v1::object_service_server::ObjectService;
use scion_proto::v1::{
    self as pb, AccessGrant, BlameRequest, CreateChangesetRequest, DiffRequest, FindLandingRequest,
    GetBlobRequest, GetChangeRequestRequest, GetRefRequest, GetTreeRequest, GrantRequest,
    InitRepoRequest, ListChangeRequestsRequest, ListGrantsRequest, LogRequest, MergeBaseRequest,
    MergeRequest, MoveRefRequest, PathLogRequest, PutBlobRequest, RebaseRequest, RevertRequest,
    RevokeRequest, SubmitChangeRequestRequest, WhoAmIRequest,
};
use scion_store::{MetadataStore, ObjectStore};
use scion_types::{
    BlobId, Changeset, ChangesetId, Decode, Encode, FileChange, FileType, Identity, RepoPath,
    Timestamp, Tree, TreeEntry,
};
use scion_web::{ANYONE, Act, Acted, Action, Task};
use serde::{Deserialize, Serialize};
use tonic::{Code, Request, Status};

use crate::MAX_MESSAGE_BYTES;
use crate::auth::{Principal, PublicReads};
use crate::control::ControlServer;
use crate::object::ObjectServer;
use crate::token::TokenKey;

#[derive(Clone)]
struct Api {
    meta: Arc<dyn MetadataStore>,
    store: Arc<dyn ObjectStore>,
    control: Arc<ControlServer>,
    objects: ObjectServer,
    key: TokenKey,
}

/// The routes, to be served under `/api/v1`.
pub(crate) fn router(
    meta: Arc<dyn MetadataStore>,
    store: Arc<dyn ObjectStore>,
    control: Arc<ControlServer>,
    objects: ObjectServer,
    key: TokenKey,
) -> Router {
    Router::new()
        .route("/repos", get(repos).post(init_repo))
        .route("/repos/{repo}/refs/{*name}", get(get_ref).put(move_ref))
        .route("/repos/{repo}/trees/{id}", get(tree))
        .route("/repos/{repo}/blobs", axum::routing::post(put_blob))
        .route("/repos/{repo}/blobs/{id}", get(blob))
        .route("/repos/{repo}/changesets", axum::routing::post(create_changeset))
        .route("/repos/{repo}/changesets/{id}", get(changeset))
        .route("/repos/{repo}/log", get(log))
        .route("/repos/{repo}/diff", get(diff))
        .route("/repos/{repo}/merge-base", get(merge_base))
        .route("/repos/{repo}/blame", get(blame))
        .route("/repos/{repo}/rebase", axum::routing::post(rebase))
        .route("/repos/{repo}/merge", axum::routing::post(merge))
        .route("/repos/{repo}/revert", axum::routing::post(revert))
        .route(
            "/repos/{repo}/change-requests",
            get(list_change_requests).post(submit_change_request),
        )
        .route("/repos/{repo}/change-requests/{id}", get(change_request))
        .route("/repos/{repo}/landings", get(landings))
        .route(
            "/repos/{repo}/grants",
            get(list_grants).put(grant).delete(revoke),
        )
        .route("/whoami", get(who_am_i))
        .route("/version", get(server_version))
        // A blob travels in one body, as in one gRPC message.
        .layer(DefaultBodyLimit::max(MAX_MESSAGE_BYTES))
        .with_state(Api {
            meta,
            store,
            control,
            objects,
            key,
        })
}

/// What the web interface's pages do, as the signed-in principal, through
/// the RPCs `scn` calls, so roles are checked and form input validated as
/// there: a land submits the request's head again and waits for the land
/// worker, a rebase replays the head onto `main`, a reject is
/// [`crate::land::reject`]; creating a repository and granting or revoking
/// a role are `scn init` and `scn access`.
pub(crate) fn page_actions(meta: Arc<dyn MetadataStore>, control: Arc<ControlServer>) -> Act {
    Arc::new(move |principal, repo, task| {
        let (meta, control) = (meta.clone(), control.clone());
        Box::pin(async move {
            // The page checked the token; its expiry is never read.
            let caller = Caller(Some(Principal(principal, SystemTime::UNIX_EPOCH)));
            page_action(&*meta, &control, &caller, repo, task)
                .await
                .map_err(|e| (e.0, e.2))
        })
    })
}

async fn page_action(
    meta: &dyn MetadataStore,
    control: &ControlServer,
    caller: &Caller,
    repo: String,
    task: Task,
) -> Result<Acted, Error> {
    let fail = |e: Status| caller.error(&e);
    let (cr, action) = match task {
        Task::Request(cr, action) => (cr, action),
        Task::Init => {
            control
                .init_repo(caller.request(InitRepoRequest { repo }))
                .await
                .map_err(fail)?;
            return Ok(Acted::Done);
        }
        Task::Grant {
            principal,
            role,
            path_prefix,
        } => {
            let grant = Some(AccessGrant {
                principal,
                path_prefix,
                role,
            });
            control
                .grant(caller.request(GrantRequest { repo, grant }))
                .await
                .map_err(fail)?;
            return Ok(Acted::Done);
        }
        Task::Revoke {
            principal,
            path_prefix,
        } => {
            let request = RevokeRequest {
                repo,
                principal,
                path_prefix,
            };
            control
                .revoke(caller.request(request))
                .await
                .map_err(fail)?;
            return Ok(Acted::Done);
        }
    };
    match action {
        Action::Land => {
            let request = SubmitChangeRequestRequest {
                repo,
                head: cr.head.as_bytes().to_vec(),
                wait: true,
            };
            let reply = control
                .submit_change_request(caller.request(request))
                .await
                .map_err(fail)?;
            Ok(Acted::Submitted(reply.into_inner().id))
        }
        Action::Rebase => {
            let main = control
                .get_ref(caller.request(GetRefRequest {
                    repo: repo.clone(),
                    name: "main".into(),
                }))
                .await
                .map_err(fail)?
                .into_inner();
            let request = RebaseRequest {
                repo,
                head: cr.head.as_bytes().to_vec(),
                onto: main.target,
            };
            let reply = control
                .rebase(caller.request(request))
                .await
                .map_err(fail)?
                .into_inner();
            Ok(match reply.head {
                Some(head) => Acted::Rebased(ChangesetId::from_bytes(
                    head.try_into().expect("the server sends 32-byte ids"),
                )),
                None => Acted::Conflicts(reply.conflicts),
            })
        }
        Action::Reject => {
            crate::land::reject(meta, &caller.request(()), &repo, &cr)
                .await
                .map_err(fail)?;
            Ok(Acted::Rejected)
        }
    }
}

/// A refusal: its HTTP status, the gRPC code's name and why.
#[derive(Debug)]
struct Error(StatusCode, &'static str, String);

type Reply<T> = Result<Json<T>, Error>;

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    message: &'a str,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: self.1,
            message: &self.2,
        };
        (self.0, Json(body)).into_response()
    }
}

fn bad_request(why: String) -> Error {
    Error(StatusCode::BAD_REQUEST, "invalid_argument", why)
}

fn unauthenticated(why: &str) -> Error {
    Error(
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
        format!(
            "{why}: send `Authorization: Bearer <token>` with a token from `scn login` or \
             `scion-server token`"
        ),
    )
}

/// Who is calling: the principal of the bearer token, or nobody.
#[derive(Debug, Clone)]
struct Caller(Option<Principal>);

impl FromRequestParts<Api> for Caller {
    type Rejection = Error;

    fn from_request_parts(
        parts: &mut Parts,
        api: &Api,
    ) -> impl Future<Output = Result<Self, Error>> + Send {
        std::future::ready(Self::of(parts, api))
    }
}

impl Caller {
    /// The caller a request's headers name; checking a token reads nothing.
    fn of(parts: &Parts, api: &Api) -> Result<Self, Error> {
        let Some(value) = parts.headers.get(header::AUTHORIZATION) else {
            if parts.method == Method::GET {
                return Ok(Self(None));
            }
            return Err(unauthenticated("no token; only reads are open without one"));
        };
        let token = value
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| unauthenticated("authorization is not `Bearer <token>`"))?;
        let (principal, expiry) = api
            .key
            .verify(token.trim(), SystemTime::now())
            .map_err(|e| unauthenticated(&e.to_string()))?;
        Ok(Self(Some(Principal(principal, expiry))))
    }

    /// The RPC request for `message`, as this caller.
    fn request<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        // No token names `anyone`; its expiry is never read, as `whoami`
        // needs a token.
        let principal = self
            .0
            .clone()
            .unwrap_or_else(|| Principal(ANYONE.to_owned(), SystemTime::UNIX_EPOCH));
        request.extensions_mut().insert(principal);
        request.extensions_mut().insert(PublicReads);
        request
    }

    /// The principal whose grants this caller holds besides [`ANYONE`]'s.
    fn principal(&self) -> Option<&str> {
        self.0.as_ref().map(|p| p.0.as_str())
    }

    /// The HTTP refusal for an RPC's `status`. A refused read without a
    /// token asks for one.
    fn error(&self, status: &Status) -> Error {
        let message = status.message().to_owned();
        let (http, name) = match status.code() {
            Code::PermissionDenied if self.0.is_none() => {
                return unauthenticated(&format!("{message}; it is not public"));
            }
            Code::InvalidArgument | Code::OutOfRange => {
                (StatusCode::BAD_REQUEST, "invalid_argument")
            }
            Code::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Code::AlreadyExists => (StatusCode::CONFLICT, "already_exists"),
            Code::FailedPrecondition | Code::Aborted => {
                (StatusCode::CONFLICT, "failed_precondition")
            }
            Code::PermissionDenied => (StatusCode::FORBIDDEN, "permission_denied"),
            Code::Unauthenticated => (StatusCode::UNAUTHORIZED, "unauthenticated"),
            Code::DeadlineExceeded => (StatusCode::GATEWAY_TIMEOUT, "deadline_exceeded"),
            Code::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            Code::DataLoss => (StatusCode::INTERNAL_SERVER_ERROR, "data_loss"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        Error(http, name, message)
    }
}

/// The 32 bytes of the hex id `s`, named `what` in a refusal.
fn id(s: &str, what: &str) -> Result<Vec<u8>, Error> {
    match hex::decode(s) {
        Ok(bytes) if bytes.len() == 32 => Ok(bytes),
        _ => Err(bad_request(format!(
            "{what} {s:?} is not an id (64 hex digits)"
        ))),
    }
}

fn path(s: &str) -> Result<RepoPath, Error> {
    RepoPath::new(s).map_err(|e| bad_request(format!("path {s:?}: {e}")))
}

/// A file's kind, as JSON spells it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Regular,
    Executable,
    Symlink,
}

impl From<FileType> for Kind {
    fn from(t: FileType) -> Self {
        match t {
            FileType::Regular => Self::Regular,
            FileType::Executable => Self::Executable,
            FileType::Symlink => Self::Symlink,
        }
    }
}

impl From<Kind> for FileType {
    fn from(k: Kind) -> Self {
        match k {
            Kind::Regular => Self::Regular,
            Kind::Executable => Self::Executable,
            Kind::Symlink => Self::Symlink,
        }
    }
}

impl Kind {
    fn of_wire(kind: i32) -> Self {
        match pb::FileType::try_from(kind) {
            Ok(pb::FileType::Executable) => Self::Executable,
            Ok(pb::FileType::Symlink) => Self::Symlink,
            _ => Self::Regular,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Person {
    name: String,
    email: String,
}

/// A time and the UTC offset it was written in, as a changeset keeps it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Date {
    unix_secs: i64,
    tz_offset_minutes: i32,
}

impl From<Timestamp> for Date {
    fn from(t: Timestamp) -> Self {
        Self {
            unix_secs: t.unix_secs,
            tz_offset_minutes: t.tz_offset_minutes,
        }
    }
}

impl From<Date> for Timestamp {
    fn from(d: Date) -> Self {
        Self::new(d.unix_secs, d.tz_offset_minutes)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CopyFrom {
    path: String,
    changeset: String,
}

/// What a changeset does to one path, against its first parent.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "lowercase")]
enum Change {
    Deleted,
    Tracked {
        content: String,
        kind: Kind,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        copy_from: Option<CopyFrom>,
    },
}

/// A changeset as JSON; the repository is the one in the URL. The server
/// builds the [`Changeset`] from it and hashes only its canonical encoding.
#[derive(Debug, Serialize, Deserialize)]
struct ChangesetJson {
    parents: Vec<String>,
    author: Person,
    author_date: Date,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    committer: Option<Person>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    committer_date: Option<Date>,
    message: String,
    #[serde(default)]
    file_changes: BTreeMap<String, Change>,
    /// Values in hex.
    #[serde(default)]
    extra: BTreeMap<String, String>,
}

impl ChangesetJson {
    fn of(cs: Changeset) -> Self {
        Self {
            parents: cs.parents.iter().map(ToString::to_string).collect(),
            author: Person {
                name: cs.author.name,
                email: cs.author.email,
            },
            author_date: cs.author_date.into(),
            committer: cs.committer.map(|c| Person {
                name: c.name,
                email: c.email,
            }),
            committer_date: cs.committer_date.map(Into::into),
            message: cs.message,
            file_changes: cs
                .file_changes
                .into_iter()
                .map(|(p, change)| {
                    let change = match change {
                        FileChange::Deleted => Change::Deleted,
                        FileChange::Tracked {
                            content,
                            kind,
                            size,
                            copy_from,
                        } => Change::Tracked {
                            content: content.to_string(),
                            kind: kind.into(),
                            size,
                            copy_from: copy_from.map(|(path, cs)| CopyFrom {
                                path: path.as_str().to_owned(),
                                changeset: cs.to_string(),
                            }),
                        },
                    };
                    (p.as_str().to_owned(), change)
                })
                .collect(),
            extra: cs
                .extra
                .into_iter()
                .map(|(k, v)| (k, hex::encode(v)))
                .collect(),
        }
    }

    /// The changeset this describes in `repo`, checked as client input.
    fn into_changeset(self, repo: &str) -> Result<Changeset, Error> {
        let changeset_id = |s: &str, what: &str| {
            s.parse::<ChangesetId>()
                .map_err(|e| bad_request(format!("{what} {s:?}: {e}")))
        };
        let parents = self
            .parents
            .iter()
            .map(|p| changeset_id(p, "parent"))
            .collect::<Result<_, _>>()?;
        let mut file_changes = BTreeMap::new();
        for (p, change) in self.file_changes {
            let change = match change {
                Change::Deleted => FileChange::Deleted,
                Change::Tracked {
                    content,
                    kind,
                    size,
                    copy_from,
                } => FileChange::Tracked {
                    content: content
                        .parse::<BlobId>()
                        .map_err(|e| bad_request(format!("content of {p:?}: {e}")))?,
                    kind: kind.into(),
                    size,
                    copy_from: match copy_from {
                        None => None,
                        Some(c) => Some((path(&c.path)?, changeset_id(&c.changeset, "copy_from")?)),
                    },
                },
            };
            file_changes.insert(path(&p)?, change);
        }
        let extra = self
            .extra
            .into_iter()
            .map(|(k, v)| {
                hex::decode(&v)
                    .map(|v| (k.clone(), v))
                    .map_err(|_| bad_request(format!("extra {k:?} is not hex")))
            })
            .collect::<Result<_, _>>()?;
        Ok(Changeset {
            repo: repo.to_owned(),
            parents,
            author: Identity::new(self.author.name, self.author.email),
            author_date: self.author_date.into(),
            committer: self.committer.map(|c| Identity::new(c.name, c.email)),
            committer_date: self.committer_date.map(Into::into),
            message: self.message,
            file_changes,
            extra,
        })
    }
}

/// A changeset with its id.
#[derive(Debug, Serialize)]
struct Shown {
    id: String,
    #[serde(flatten)]
    changeset: ChangesetJson,
}

fn shown(encoded: &[u8]) -> Result<Shown, Error> {
    let cs = Changeset::decode(encoded).map_err(|e| {
        Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("stored changeset: {e}"),
        )
    })?;
    Ok(Shown {
        id: cs.id().to_string(),
        changeset: ChangesetJson::of(cs),
    })
}

#[derive(Debug, Serialize)]
struct ChangeRequestJson {
    id: u64,
    head: String,
    base: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    landed: Option<String>,
    conflicts: Vec<String>,
    submitted_unix_micros: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    landed_unix_micros: Option<u64>,
}

fn status_name(status: pb::ChangeRequestStatus) -> &'static str {
    use pb::ChangeRequestStatus as S;
    match status {
        S::Unspecified => "unspecified",
        S::Pending => "pending",
        S::Landing => "landing",
        S::Landed => "landed",
        S::NeedsRebase => "needs_rebase",
        S::Rejected => "rejected",
    }
}

impl From<pb::ChangeRequest> for ChangeRequestJson {
    fn from(cr: pb::ChangeRequest) -> Self {
        Self {
            id: cr.id,
            head: hex::encode(&cr.head),
            base: hex::encode(&cr.base),
            status: status_name(cr.status()),
            landed: cr.landed.as_deref().map(hex::encode),
            conflicts: cr.conflicts,
            submitted_unix_micros: cr.submitted_unix_micros,
            landed_unix_micros: cr.landed_unix_micros,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct GrantJson {
    principal: String,
    /// The directory the role holds under; absent or empty for the whole
    /// repository.
    #[serde(default)]
    path: String,
    role: String,
}

impl From<AccessGrant> for GrantJson {
    fn from(g: AccessGrant) -> Self {
        Self {
            principal: g.principal,
            path: g.path_prefix,
            role: g.role,
        }
    }
}

#[derive(Debug, Serialize)]
struct Repos {
    repos: Vec<String>,
}

/// The repositories the caller holds a grant in, and the public ones.
async fn repos(State(api): State<Api>, caller: Caller) -> Reply<Repos> {
    let store = |e: scion_store::StoreError| caller.error(&crate::store_status(e));
    let mut repos = api.meta.repos_of(ANYONE).await.map_err(store)?;
    if let Some(p) = caller.principal() {
        repos.extend(api.meta.repos_of(p).await.map_err(store)?);
        repos.sort();
        repos.dedup();
    }
    Ok(Json(Repos { repos }))
}

#[derive(Debug, Deserialize)]
struct NewRepo {
    repo: String,
}

#[derive(Debug, Serialize)]
struct RefJson {
    target: String,
    version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_tree: Option<String>,
}

async fn init_repo(
    State(api): State<Api>,
    caller: Caller,
    Json(body): Json<NewRepo>,
) -> Result<(StatusCode, Json<RefJson>), Error> {
    let reply = api
        .control
        .init_repo(caller.request(InitRepoRequest { repo: body.repo }))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok((
        StatusCode::CREATED,
        Json(RefJson {
            target: hex::encode(reply.main),
            version: reply.version,
            root_tree: None,
        }),
    ))
}

async fn get_ref(
    State(api): State<Api>,
    caller: Caller,
    Path((repo, name)): Path<(String, String)>,
) -> Reply<RefJson> {
    let reply = api
        .control
        .get_ref(caller.request(GetRefRequest { repo, name }))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(RefJson {
        target: hex::encode(reply.target),
        version: reply.version,
        root_tree: Some(hex::encode(reply.root_tree)),
    }))
}

#[derive(Debug, Deserialize)]
struct MoveRef {
    target: String,
    /// Absent to create the ref.
    expected_version: Option<u64>,
}

#[derive(Debug, Serialize)]
struct Version {
    version: u64,
}

async fn move_ref(
    State(api): State<Api>,
    caller: Caller,
    Path((repo, name)): Path<(String, String)>,
    Json(body): Json<MoveRef>,
) -> Reply<Version> {
    let request = MoveRefRequest {
        repo,
        name,
        target: id(&body.target, "target")?,
        expected_version: body.expected_version,
    };
    let reply = api
        .control
        .move_ref(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(Version {
        version: reply.version,
    }))
}

#[derive(Debug, Serialize)]
struct EntryJson {
    name: String,
    #[serde(rename = "type")]
    ty: &'static str,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<Kind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
}

#[derive(Debug, Serialize)]
struct TreeJson {
    id: String,
    entries: Vec<EntryJson>,
}

async fn tree(
    State(api): State<Api>,
    caller: Caller,
    Path((repo, tree)): Path<(String, String)>,
) -> Reply<TreeJson> {
    let request = GetTreeRequest {
        id: id(&tree, "tree")?,
        repo,
    };
    let reply = api
        .objects
        .get_tree(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    let decoded = Tree::decode(&reply.tree).map_err(|e| {
        Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("stored tree: {e}"),
        )
    })?;
    let entries = decoded
        .entries
        .into_iter()
        .map(|(name, entry)| {
            let name = name.as_str().to_owned();
            match entry {
                TreeEntry::Tree { id } => EntryJson {
                    name,
                    ty: "tree",
                    id: id.to_string(),
                    kind: None,
                    size: None,
                },
                TreeEntry::Blob { id, kind, size } => EntryJson {
                    name,
                    ty: "blob",
                    id: id.to_string(),
                    kind: Some(kind.into()),
                    size: Some(size),
                },
            }
        })
        .collect();
    Ok(Json(TreeJson { id: tree, entries }))
}

/// A blob's bytes.
async fn blob(
    State(api): State<Api>,
    caller: Caller,
    Path((repo, blob)): Path<(String, String)>,
) -> Result<Response, Error> {
    let request = GetBlobRequest {
        id: id(&blob, "blob")?,
        repo,
    };
    let reply = api
        .objects
        .get_blob(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        reply.data,
    )
        .into_response())
}

#[derive(Debug, Serialize)]
struct Id {
    id: String,
}

/// Store the body as a blob.
async fn put_blob(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    body: Bytes,
) -> Result<(StatusCode, Json<Id>), Error> {
    let request = PutBlobRequest {
        data: body.to_vec(),
        repo,
    };
    let reply = api
        .objects
        .put_blob(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok((
        StatusCode::CREATED,
        Json(Id {
            id: hex::encode(reply.id),
        }),
    ))
}

#[derive(Debug, Serialize)]
struct Created {
    id: String,
    root_tree: String,
}

async fn create_changeset(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Json(body): Json<ChangesetJson>,
) -> Result<(StatusCode, Json<Created>), Error> {
    let cs = body.into_changeset(&repo)?;
    let request = CreateChangesetRequest {
        changeset: cs.encode_to_vec(),
    };
    let reply = api
        .control
        .create_changeset(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok((
        StatusCode::CREATED,
        Json(Created {
            id: hex::encode(reply.id),
            root_tree: hex::encode(reply.root_tree),
        }),
    ))
}

/// One changeset, read as the first entry of its own log.
async fn changeset(
    State(api): State<Api>,
    caller: Caller,
    Path((repo, cs)): Path<(String, String)>,
) -> Reply<WithTree> {
    let head = id(&cs, "changeset")?;
    let request = LogRequest {
        repo: repo.clone(),
        head: head.clone(),
        limit: 1,
    };
    let reply = api
        .control
        .log(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    let first = reply.changesets.first().ok_or_else(|| {
        Error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("changeset {cs}"),
        )
    })?;
    let shown = shown(first)?;
    // The log above authorized the read and found the changeset.
    let cs =
        ChangesetId::try_from_slice(&head).map_err(|e| bad_request(format!("changeset: {e}")))?;
    let root = Deriver::new(&repo, &*api.meta, &*api.store)
        .derive::<RootTree>(cs)
        .await
        .map_err(|e| {
            Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "data_loss",
                e.to_string(),
            )
        })?;
    Ok(Json(WithTree {
        shown,
        root_tree: root.0.to_string(),
    }))
}

/// A changeset with the root tree it leaves, so a client can read any path
/// at it.
#[derive(Debug, Serialize)]
struct WithTree {
    #[serde(flatten)]
    shown: Shown,
    root_tree: String,
}

#[derive(Debug, Deserialize)]
struct LogQuery {
    head: String,
    limit: Option<u32>,
    path: Option<String>,
}

#[derive(Debug, Serialize)]
struct Changesets {
    changesets: Vec<Shown>,
}

/// Changesets a log returns when the query names no limit.
const DEFAULT_LOG_LIMIT: u32 = 50;

async fn log(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Query(q): Query<LogQuery>,
) -> Reply<Changesets> {
    let head = id(&q.head, "head")?;
    let limit = q.limit.unwrap_or(DEFAULT_LOG_LIMIT);
    let reply = match q.path {
        None => {
            api.control
                .log(caller.request(LogRequest { repo, head, limit }))
                .await
        }
        Some(path) => {
            let request = PathLogRequest {
                repo,
                head,
                path,
                limit,
            };
            api.control.path_log(caller.request(request)).await
        }
    }
    .map_err(|e| caller.error(&e))?
    .into_inner();
    let changesets = reply
        .changesets
        .iter()
        .map(|cs| shown(cs))
        .collect::<Result<_, _>>()?;
    Ok(Json(Changesets { changesets }))
}

#[derive(Debug, Deserialize)]
struct DiffQuery {
    old: Option<String>,
    new: String,
}

#[derive(Debug, Serialize)]
struct FileJson {
    id: String,
    kind: Kind,
    size: u64,
}

#[derive(Debug, Serialize)]
struct DiffEntryJson {
    path: String,
    old: Option<FileJson>,
    new: Option<FileJson>,
}

#[derive(Debug, Serialize)]
struct DiffJson {
    entries: Vec<DiffEntryJson>,
}

fn file(f: pb::FileEntry) -> FileJson {
    FileJson {
        id: hex::encode(f.id),
        kind: Kind::of_wire(f.kind),
        size: f.size,
    }
}

async fn diff(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Query(q): Query<DiffQuery>,
) -> Reply<DiffJson> {
    let request = DiffRequest {
        repo,
        old: q.old.as_deref().map(|o| id(o, "old")).transpose()?,
        new: id(&q.new, "new")?,
    };
    let reply = api
        .control
        .diff(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(DiffJson {
        entries: reply
            .entries
            .into_iter()
            .map(|e| DiffEntryJson {
                path: e.path,
                old: e.old.map(file),
                new: e.new.map(file),
            })
            .collect(),
    }))
}

#[derive(Debug, Deserialize)]
struct MergeBaseQuery {
    a: String,
    b: String,
}

#[derive(Debug, Serialize)]
struct Base {
    base: Option<String>,
}

async fn merge_base(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Query(q): Query<MergeBaseQuery>,
) -> Reply<Base> {
    let request = MergeBaseRequest {
        repo,
        a: id(&q.a, "a")?,
        b: id(&q.b, "b")?,
    };
    let reply = api
        .control
        .merge_base(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(Base {
        base: reply.base.map(hex::encode),
    }))
}

#[derive(Debug, Deserialize)]
struct BlameQuery {
    changeset: String,
    path: String,
}

#[derive(Debug, Serialize)]
struct RunJson {
    changeset: String,
    lines: u64,
}

#[derive(Debug, Serialize)]
struct BlameJson {
    runs: Vec<RunJson>,
}

async fn blame(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Query(q): Query<BlameQuery>,
) -> Reply<BlameJson> {
    let request = BlameRequest {
        repo,
        changeset: id(&q.changeset, "changeset")?,
        path: q.path,
    };
    let reply = api
        .control
        .blame(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(BlameJson {
        runs: reply
            .runs
            .into_iter()
            .map(|r| RunJson {
                changeset: hex::encode(r.changeset),
                lines: r.lines,
            })
            .collect(),
    }))
}

#[derive(Debug, Deserialize)]
struct RebaseBody {
    head: String,
    onto: String,
}

/// The result of a rebase, merge or revert: the changeset it gives, or the
/// paths that stopped it.
#[derive(Debug, Serialize)]
struct Outcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    changeset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_tree: Option<String>,
    /// Changesets the call created.
    created: u32,
    /// The changeset a rebase could not replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    conflicted: Option<String>,
    /// Conflicting paths, or for a revert the paths changed since.
    conflicts: Vec<String>,
}

/// `root_tree` if `changeset` is present: the wire sends empty bytes without.
fn tree_of(changeset: Option<&Vec<u8>>, root: &[u8]) -> Option<String> {
    changeset.map(|_| hex::encode(root))
}

async fn rebase(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Json(body): Json<RebaseBody>,
) -> Reply<Outcome> {
    let request = RebaseRequest {
        repo,
        head: id(&body.head, "head")?,
        onto: id(&body.onto, "onto")?,
    };
    let reply = api
        .control
        .rebase(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(Outcome {
        root_tree: tree_of(reply.head.as_ref(), &reply.root_tree),
        changeset: reply.head.map(hex::encode),
        created: reply.created,
        conflicted: (!reply.conflicted.is_empty()).then(|| hex::encode(&reply.conflicted)),
        conflicts: reply.conflicts,
    }))
}

/// Who writes a merge or revert, when, and why.
#[derive(Debug, Deserialize)]
struct Authored {
    author: Person,
    /// Now, in UTC, if absent.
    date: Option<Date>,
    message: String,
}

impl Authored {
    fn date(&self) -> Date {
        self.date.unwrap_or_else(|| Date {
            unix_secs: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
            tz_offset_minutes: 0,
        })
    }
}

#[derive(Debug, Deserialize)]
struct MergeBody {
    ours: String,
    theirs: String,
    #[serde(flatten)]
    authored: Authored,
}

async fn merge(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Json(body): Json<MergeBody>,
) -> Reply<Outcome> {
    let date = body.authored.date();
    let request = MergeRequest {
        repo,
        ours: id(&body.ours, "ours")?,
        theirs: id(&body.theirs, "theirs")?,
        author_name: body.authored.author.name,
        author_email: body.authored.author.email,
        author_unix_secs: date.unix_secs,
        author_tz_offset_minutes: date.tz_offset_minutes,
        message: body.authored.message,
    };
    let reply = api
        .control
        .merge(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(Outcome {
        root_tree: tree_of(reply.merged.as_ref(), &reply.root_tree),
        changeset: reply.merged.map(hex::encode),
        created: u32::from(reply.created),
        conflicted: None,
        conflicts: reply.conflicts,
    }))
}

#[derive(Debug, Deserialize)]
struct RevertBody {
    head: String,
    changeset: String,
    #[serde(flatten)]
    authored: Authored,
}

async fn revert(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Json(body): Json<RevertBody>,
) -> Reply<Outcome> {
    let date = body.authored.date();
    let request = RevertRequest {
        repo,
        head: id(&body.head, "head")?,
        changeset: id(&body.changeset, "changeset")?,
        author_name: body.authored.author.name,
        author_email: body.authored.author.email,
        author_unix_secs: date.unix_secs,
        author_tz_offset_minutes: date.tz_offset_minutes,
        message: body.authored.message,
    };
    let reply = api
        .control
        .revert(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(Outcome {
        root_tree: tree_of(reply.reverted.as_ref(), &reply.root_tree),
        created: u32::from(reply.reverted.is_some()),
        changeset: reply.reverted.map(hex::encode),
        conflicted: None,
        conflicts: reply.changed_since,
    }))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    status: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ChangeRequests {
    requests: Vec<ChangeRequestJson>,
}

/// Change requests a list returns when the query names no limit.
const DEFAULT_LIST_LIMIT: u32 = 100;

async fn list_change_requests(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Query(q): Query<ListQuery>,
) -> Reply<ChangeRequests> {
    use pb::ChangeRequestStatus as S;
    let status = match q.status.as_deref() {
        None => S::Unspecified,
        Some(name) => [
            S::Pending,
            S::Landing,
            S::Landed,
            S::NeedsRebase,
            S::Rejected,
        ]
        .into_iter()
        .find(|s| status_name(*s) == name)
        .ok_or_else(|| {
            bad_request(format!(
                "status {name:?}; one of pending, landing, landed, needs_rebase, rejected"
            ))
        })?,
    };
    let request = ListChangeRequestsRequest {
        repo,
        status: status.into(),
        limit: q.limit.unwrap_or(DEFAULT_LIST_LIMIT),
    };
    let reply = api
        .control
        .list_change_requests(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(ChangeRequests {
        requests: reply.requests.into_iter().map(Into::into).collect(),
    }))
}

#[derive(Debug, Deserialize)]
struct Submit {
    head: String,
    /// Reply once the request is finished, or after at most 20 seconds.
    #[serde(default)]
    wait: bool,
}

async fn submit_change_request(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Json(body): Json<Submit>,
) -> Result<(StatusCode, Json<ChangeRequestJson>), Error> {
    let request = SubmitChangeRequestRequest {
        repo,
        head: id(&body.head, "head")?,
        wait: body.wait,
    };
    let reply = api
        .control
        .submit_change_request(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok((StatusCode::CREATED, Json(reply.into())))
}

#[derive(Debug, Deserialize)]
struct Wait {
    #[serde(default)]
    wait: bool,
}

async fn change_request(
    State(api): State<Api>,
    caller: Caller,
    Path((repo, request_id)): Path<(String, u64)>,
    Query(q): Query<Wait>,
) -> Reply<ChangeRequestJson> {
    let request = GetChangeRequestRequest {
        repo,
        id: request_id,
        wait: q.wait,
    };
    let reply = api
        .control
        .get_change_request(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(reply.into()))
}

#[derive(Debug, Deserialize)]
struct LandingsQuery {
    /// Changeset ids, comma-separated.
    changesets: String,
}

#[derive(Debug, Serialize)]
struct LandingJson {
    changeset: String,
    request: ChangeRequestJson,
}

#[derive(Debug, Serialize)]
struct Landings {
    landings: Vec<LandingJson>,
}

async fn landings(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Query(q): Query<LandingsQuery>,
) -> Reply<Landings> {
    let changesets = q
        .changesets
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| id(s, "changeset"))
        .collect::<Result<_, _>>()?;
    let reply = api
        .control
        .find_landing(caller.request(FindLandingRequest { repo, changesets }))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(Landings {
        landings: reply
            .landings
            .into_iter()
            .filter_map(|l| {
                Some(LandingJson {
                    changeset: hex::encode(l.changeset),
                    request: l.request?.into(),
                })
            })
            .collect(),
    }))
}

#[derive(Debug, Serialize)]
struct Grants {
    grants: Vec<GrantJson>,
}

async fn list_grants(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
) -> Reply<Grants> {
    let reply = api
        .control
        .list_grants(caller.request(ListGrantsRequest { repo }))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(Grants {
        grants: reply.grants.into_iter().map(Into::into).collect(),
    }))
}

async fn grant(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Json(body): Json<GrantJson>,
) -> Result<StatusCode, Error> {
    let request = GrantRequest {
        repo,
        grant: Some(AccessGrant {
            principal: body.principal,
            path_prefix: body.path,
            role: body.role,
        }),
    };
    api.control
        .grant(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct RevokeQuery {
    principal: String,
    #[serde(default)]
    path: String,
}

async fn revoke(
    State(api): State<Api>,
    caller: Caller,
    Path(repo): Path<String>,
    Query(q): Query<RevokeQuery>,
) -> Result<StatusCode, Error> {
    let request = RevokeRequest {
        repo,
        principal: q.principal,
        path_prefix: q.path,
    };
    api.control
        .revoke(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct WhoAmIQuery {
    repo: Option<String>,
}

#[derive(Debug, Serialize)]
struct Me {
    principal: String,
    expires_unix_seconds: i64,
    grants: Vec<GrantJson>,
}

#[derive(Debug, Serialize)]
struct ServerVersion {
    version: &'static str,
}

/// The server's version, to anyone: the web interface's footer shows it too.
async fn server_version() -> Json<ServerVersion> {
    Json(ServerVersion {
        version: scion_types::version(),
    })
}

async fn who_am_i(
    State(api): State<Api>,
    caller: Caller,
    Query(q): Query<WhoAmIQuery>,
) -> Reply<Me> {
    if caller.0.is_none() {
        return Err(unauthenticated("no token"));
    }
    let request = WhoAmIRequest {
        repo: q.repo.unwrap_or_default(),
    };
    let reply = api
        .control
        .who_am_i(caller.request(request))
        .await
        .map_err(|e| caller.error(&e))?
        .into_inner();
    Ok(Json(Me {
        principal: reply.principal,
        expires_unix_seconds: reply.expires_unix_seconds,
        grants: reply.grants.into_iter().map(Into::into).collect(),
    }))
}
