//! `scion-web`: the web interface. Server-rendered pages over the metadata
//! and object stores, run in process by `scion-server --web`, so a page reads
//! what the control plane reads with no RPC in between.
//!
//! Every page except the repository list needs a reader of the repository it
//! shows, from the same `grants` table the control plane checks. The caller's
//! token arrives as `Authorization: Bearer <token>` or in the session cookie
//! a GitHub sign-in sets ([`Login`]), and is checked by the [`Verify`]
//! function the server passes in. A visitor with neither reads what
//! [`ANYONE`] is granted, and so does everyone else besides their own
//! grants: a reader grant to it makes a repository, or a directory, public.
//! As on the control plane, a directory-scoped reader sees log and blame only
//! for paths under its grants and a changeset's diff only for those paths,
//! while trees and files are objects and need a reader of the repository
//! only.

#![forbid(unsafe_code)]

mod html;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::serve::ListenerExt;
use futures_util::future::BoxFuture;
use futures_util::{StreamExt, TryStreamExt, stream};
use maud::Markup;
use scion_derived::{Blame, DeriveError, Deriver, RootTree};
use scion_store::{MetadataStore, ObjectStore, StoreError, is_allowed_ref_name};
use scion_types::{Changeset, ChangesetId, RepoPath, TreeEntry};
use serde::Deserialize;
use tokio::net::TcpListener;

/// Checks a bearer token and returns the principal it names, or why not.
pub type Verify = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// Turns the `code` GitHub sends back after a sign-in into a token and how
/// long it is valid, or says why not.
pub type Exchange =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<(String, Duration), String>> + Send + Sync>;

/// The principal whose grants every visitor reads: a reader grant to it
/// makes a repository, or a directory with `--path`, public. No token names
/// it, and it can be granted nothing but `reader`.
pub const ANYONE: &str = "anyone";

/// GitHub's web flow, run by the server: the pages send a browser to
/// `authorize` with a fresh `state`, and on its return to `/login/callback`
/// trade the code for a token kept in the session cookie.
#[derive(Clone)]
pub struct Login {
    /// GitHub's authorize URL for the server's OAuth app, with its query
    /// begun; the page appends `&state=`.
    pub authorize: String,
    /// A value nobody can guess, new on every call, for one sign-in's state.
    pub nonce: Arc<dyn Fn() -> String + Send + Sync>,
    /// The token for the code GitHub sends back.
    pub exchange: Exchange,
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login")
            .field("authorize", &self.authorize)
            .finish_non_exhaustive()
    }
}

/// The cookie that holds a signed-in browser's token.
const SESSION_COOKIE: &str = "scion_session";

/// The cookie that holds a sign-in's state until GitHub sends it back.
const STATE_COOKIE: &str = "scion_login";

/// How long a browser has to finish signing in on GitHub.
const STATE_AGE: Duration = Duration::from_secs(600);

/// Upper bound on every page, as on every RPC.
const DEADLINE: Duration = Duration::from_secs(30);

/// Changesets one log page lists.
const LOG_PAGE: usize = 50;

/// Change requests the list shows, newest first.
const REQUESTS_PAGE: u32 = 100;

/// Largest file a page shows or diffs line by line.
const MAX_SHOWN_BYTES: u64 = 1 << 20;

/// Files of a changeset shown with their line diff; the rest are listed.
const MAX_DIFFED_FILES: usize = 20;

/// Blobs a changeset page reads at once.
const READ_CONCURRENCY: usize = 8;

/// The ref a page shows when the URL names none.
const MAIN: &str = "main";

#[derive(Clone)]
struct Web {
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    verify: Verify,
    login: Option<Login>,
}

/// The pages, for a server that runs its own listener; browsers sign in
/// through `login`, if given.
pub fn router(
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    verify: Verify,
    login: Option<Login>,
) -> Router {
    Router::new()
        .route("/", get(repos))
        .route("/login", get(sign_in))
        .route("/login/callback", get(callback))
        .route("/logout", post(sign_out))
        .route("/{repo}", get(home))
        .route("/{repo}/tree", get(tree))
        .route("/{repo}/log", get(log))
        .route("/{repo}/blame", get(blame))
        .route("/{repo}/changeset/{id}", get(changeset))
        .route("/{repo}/requests", get(requests))
        .route("/{repo}/requests/{id}", get(request))
        .layer(middleware::from_fn(guard))
        .with_state(Web {
            meta,
            objects,
            verify,
            login,
        })
}

/// Serve [`router`] on `listener` until the future is dropped or accepting
/// fails.
pub async fn serve(
    listener: TcpListener,
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    verify: Verify,
    login: Option<Login>,
) -> std::io::Result<()> {
    // Without TCP_NODELAY a small reply waits for the client's delayed ACK.
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    axum::serve(listener, router(meta, objects, verify, login)).await
}

/// Every page gets [`DEADLINE`], and a policy that lets a page load nothing
/// but its own inline style: rendered READMEs come from repository content.
async fn guard(request: Request, next: Next) -> Response {
    let mut response = match tokio::time::timeout(DEADLINE, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            Error(StatusCode::GATEWAY_TIMEOUT, "the page took too long".into()).into_response()
        }
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; img-src https:"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// A page that could not be shown: its status and why.
#[derive(Debug)]
struct Error(StatusCode, String);

type Page = Result<Markup, Error>;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, html::error(self.0, &self.1)).into_response()
    }
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        let status = match e {
            StoreError::RepoNotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, e.to_string())
    }
}

impl From<DeriveError> for Error {
    fn from(e: DeriveError) -> Self {
        match e {
            DeriveError::MissingChangeset(id) => not_found(format!("no changeset {id}")),
            DeriveError::Store(e) => e.into(),
            e => Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
}

fn not_found(why: String) -> Error {
    Error(StatusCode::NOT_FOUND, why)
}

fn bad_request(why: String) -> Error {
    Error(StatusCode::BAD_REQUEST, why)
}

/// Who is asking: the principal of the request's bearer token, else of its
/// session cookie, else nobody. A bad bearer token is refused; a session
/// that no longer verifies (it expired, or the key changed) counts as none.
fn viewer(web: &Web, headers: &HeaderMap) -> Result<Option<String>, Error> {
    let unauthenticated = |why: &str| {
        Error(
            StatusCode::UNAUTHORIZED,
            format!(
                "{why}: send `Authorization: Bearer <token>` with a token from `scn login` or \
                 `scion-server token`"
            ),
        )
    };
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        let token = value
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| unauthenticated("authorization is not `Bearer <token>`"))?;
        return (web.verify)(token.trim())
            .map(Some)
            .map_err(|e| unauthenticated(&e));
    }
    Ok(cookie(headers, SESSION_COOKIE).and_then(|token| (web.verify)(token).ok()))
}

/// The value of cookie `name` in the request, if it sent one.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k == name && !v.is_empty()).then_some(v)
        })
}

/// A `Set-Cookie` value for `name`, readable only by the server, sent only
/// over HTTPS (or to a loopback address) and on top-level navigation from
/// other sites; an empty `value` with no age removes it.
fn set_cookie(name: &str, value: &str, path: &str, age: Duration) -> HeaderValue {
    HeaderValue::try_from(format!(
        "{name}={value}; Path={path}; Max-Age={}; HttpOnly; Secure; SameSite=Lax",
        age.as_secs()
    ))
    .expect("tokens and nonces are hex")
}

/// What a reader may see of one repository.
struct Reader {
    principal: String,
    /// Directories of its grants, without a trailing `/`; empty for the
    /// whole repository.
    prefixes: Vec<String>,
}

impl Reader {
    /// Whether `path` is under one of the reader's directories.
    fn allows(&self, path: &RepoPath) -> bool {
        let path = path.as_str();
        self.prefixes.iter().any(|p| {
            p.is_empty()
                || path
                    .strip_prefix(p.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }

    /// Refuse unless the reader may read `path`.
    fn check(&self, repo: &str, path: &RepoPath) -> Result<(), Error> {
        if self.allows(path) {
            return Ok(());
        }
        Err(Error(
            StatusCode::FORBIDDEN,
            format!(
                "{} may not read {path}: its grants in {repo:?} cover {}/",
                self.principal,
                self.prefixes.join("/, ")
            ),
        ))
    }
}

/// The request's principal, refused unless it, or [`ANYONE`], reads `repo`
/// on some path.
async fn reader(web: &Web, headers: &HeaderMap, repo: &str) -> Result<Reader, Error> {
    let principal = viewer(web, headers)?;
    // One read for both principals: a repository has a grant per member.
    let prefixes: Vec<String> = web
        .meta
        .grants(repo)
        .await?
        .into_iter()
        .filter(|g| g.principal == ANYONE || Some(&g.principal) == principal.as_ref())
        .map(|g| g.path_prefix)
        .collect();
    match principal {
        // Also for a repository that does not exist, as on the control plane.
        None if prefixes.is_empty() => Err(Error(
            StatusCode::UNAUTHORIZED,
            format!(
                "{repo:?} is not public: sign in at /login, or send `Authorization: Bearer \
                 <token>` with a token from `scn login` or `scion-server token`"
            ),
        )),
        Some(principal) if prefixes.is_empty() => Err(Error(
            StatusCode::FORBIDDEN,
            format!(
                "{principal} is not a reader of {repo:?}; an admin of it can run \
                 `scn access grant {principal} reader`"
            ),
        )),
        principal => Ok(Reader {
            principal: principal.unwrap_or_else(|| ANYONE.to_owned()),
            prefixes,
        }),
    }
}

/// Where a page looks: a revision (a ref or a changeset id, `main` if
/// absent) and a path (the root if absent).
#[derive(Debug, Deserialize)]
struct Loc {
    at: Option<String>,
    path: Option<String>,
}

impl Loc {
    fn at(&self) -> &str {
        self.at.as_deref().filter(|a| !a.is_empty()).unwrap_or(MAIN)
    }

    fn path(&self) -> Result<Option<RepoPath>, Error> {
        match self.path.as_deref().map(|p| p.trim_matches('/')) {
            None | Some("") => Ok(None),
            Some(p) => RepoPath::new(p)
                .map(Some)
                .map_err(|e| bad_request(format!("path {p:?}: {e}"))),
        }
    }
}

/// The changeset `at` names in `repo`: a changeset id, or a ref.
async fn resolve(web: &Web, repo: &str, at: &str) -> Result<ChangesetId, Error> {
    if let Ok(id) = at.parse() {
        return Ok(id);
    }
    if !is_allowed_ref_name(at) {
        return Err(bad_request(format!(
            "{at:?} is neither a changeset id nor a ref (`main`, `release/*`)"
        )));
    }
    let state = web
        .meta
        .get_ref(repo, at)
        .await?
        .ok_or_else(|| not_found(format!("no ref {at:?} in {repo:?}")))?;
    Ok(state.target)
}

fn changeset_id(s: &str) -> Result<ChangesetId, Error> {
    s.parse()
        .map_err(|e| bad_request(format!("changeset {s:?}: {e}")))
}

/// The changesets `ids` of `repo`, in order, in one read.
async fn changesets(web: &Web, repo: &str, ids: &[ChangesetId]) -> Result<Vec<Changeset>, Error> {
    web.meta
        .get_changesets(repo, ids)
        .await?
        .into_iter()
        .zip(ids)
        .map(|(cs, id)| {
            cs.ok_or_else(|| {
                Error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("indexed changeset {id} is missing"),
                )
            })
        })
        .collect()
}

/// A file's bytes, as a page shows them: text up to [`MAX_SHOWN_BYTES`].
enum Content {
    Text(String),
    Binary,
    TooLarge,
}

impl Content {
    /// Reads `id` unless `size` is over [`MAX_SHOWN_BYTES`].
    async fn read(web: &Web, id: scion_types::BlobId, size: u64) -> Result<Self, Error> {
        if size > MAX_SHOWN_BYTES {
            return Ok(Self::TooLarge);
        }
        let bytes = web.objects.get_blob(id).await?.ok_or_else(|| {
            Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("blob {id} is missing"),
            )
        })?;
        Ok(match String::from_utf8(bytes.to_vec()) {
            Ok(text) if !text.contains('\0') => Self::Text(text),
            _ => Self::Binary,
        })
    }
}

/// The repositories the caller holds a grant in, and the public ones.
async fn repos(State(web): State<Web>, headers: HeaderMap) -> Page {
    let principal = viewer(&web, &headers)?;
    let public = web.meta.repos_of(ANYONE);
    let repos = match &principal {
        None => public.await?,
        Some(p) => {
            let (mut own, public) = futures_util::try_join!(web.meta.repos_of(p), public)?;
            own.extend(public);
            own.sort();
            own.dedup();
            own
        }
    };
    Ok(html::repos(
        principal.as_deref(),
        web.login.is_some(),
        &repos,
    ))
}

/// Send the browser to GitHub to sign in, remembering the sign-in's state
/// in a cookie that only `/login/callback` reads.
async fn sign_in(State(web): State<Web>) -> Result<Response, Error> {
    let login = web.login.as_ref().ok_or_else(|| {
        not_found(
            "this server has no GitHub sign-in (SCION_GITHUB_CLIENT_ID and \
             SCION_GITHUB_CLIENT_SECRET are not both set)"
                .into(),
        )
    })?;
    let state = (login.nonce)();
    let to = HeaderValue::try_from(format!("{}&state={state}", login.authorize))
        .map_err(|e| Error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok((
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, to),
            (
                header::SET_COOKIE,
                set_cookie(STATE_COOKIE, &state, "/login", STATE_AGE),
            ),
        ],
    )
        .into_response())
}

/// What GitHub sends a browser back with.
#[derive(Debug, Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error_description: Option<String>,
}

/// Finish a sign-in: the state must be the one this browser was sent off
/// with, so no other site can sign it in as someone else; the code becomes
/// a token in the session cookie.
async fn callback(
    State(web): State<Web>,
    Query(back): Query<Callback>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let login = web
        .login
        .as_ref()
        .ok_or_else(|| not_found("this server has no GitHub sign-in".into()))?;
    if let Some(why) = back.error_description {
        return Err(Error(
            StatusCode::FORBIDDEN,
            format!("GitHub did not sign you in: {why}; try again at /login"),
        ));
    }
    let (Some(code), Some(state)) = (back.code, back.state) else {
        return Err(bad_request("GitHub sent no code and state".into()));
    };
    if cookie(&headers, STATE_COOKIE) != Some(state.as_str()) {
        return Err(bad_request(
            "this sign-in was not started from this browser, or took longer than ten minutes; \
             start again at /login"
                .into(),
        ));
    }
    let (token, age) = (login.exchange)(code)
        .await
        .map_err(|e| Error(StatusCode::BAD_GATEWAY, e))?;
    let mut response = (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
    let headers = response.headers_mut();
    headers.append(
        header::SET_COOKIE,
        set_cookie(SESSION_COOKIE, &token, "/", age),
    );
    headers.append(
        header::SET_COOKIE,
        set_cookie(STATE_COOKIE, "", "/login", Duration::ZERO),
    );
    Ok(response)
}

/// Forget the session; the token itself stays valid until it expires.
async fn sign_out() -> Response {
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, HeaderValue::from_static("/")),
            (
                header::SET_COOKIE,
                set_cookie(SESSION_COOKIE, "", "/", Duration::ZERO),
            ),
        ],
    )
        .into_response()
}

/// `main`'s top directory and its README, with the repository's refs.
async fn home(State(web): State<Web>, Path(repo): Path<String>, headers: HeaderMap) -> Page {
    reader(&web, &headers, &repo).await?;
    let refs = web.meta.list_refs(&repo).await?;
    let Some((_, main)) = refs.iter().find(|(name, _)| name == MAIN) else {
        return Err(not_found(format!("{repo:?} has no {MAIN}")));
    };
    let deriver = Deriver::new(&repo, &*web.meta, &*web.objects);
    let root = deriver.derive::<RootTree>(main.target).await?;
    let tree = web
        .objects
        .get_tree(root.0)
        .await?
        .ok_or(DeriveError::MissingTree(root.0))?;
    let readme = tree.entries.iter().find_map(|(name, entry)| match entry {
        TreeEntry::Blob { id, size, .. } if html::is_readme(name.as_str()) => {
            Some((name.as_str().to_owned(), *id, *size))
        }
        _ => None,
    });
    let readme = match readme {
        Some((name, id, size)) => Some((name, Content::read(&web, id, size).await?)),
        None => None,
    };
    let refs: Vec<String> = refs.into_iter().map(|(name, _)| name).collect();
    Ok(html::home(&repo, &refs, &tree, readme.as_ref()))
}

/// A directory listing or a file, at a revision.
async fn tree(
    State(web): State<Web>,
    Path(repo): Path<String>,
    Query(loc): Query<Loc>,
    headers: HeaderMap,
) -> Page {
    reader(&web, &headers, &repo).await?;
    let path = loc.path()?;
    let cs = resolve(&web, &repo, loc.at()).await?;
    let deriver = Deriver::new(&repo, &*web.meta, &*web.objects);
    let root = deriver.derive::<RootTree>(cs).await?;
    let entry = match &path {
        None => TreeEntry::Tree { id: root.0 },
        Some(p) => root
            .lookup(&*web.objects, p)
            .await?
            .ok_or_else(|| not_found(format!("no {p} at {}", loc.at())))?,
    };
    match entry {
        TreeEntry::Tree { id } => {
            let tree = web
                .objects
                .get_tree(id)
                .await?
                .ok_or(DeriveError::MissingTree(id))?;
            Ok(html::directory(&repo, loc.at(), path.as_ref(), &tree))
        }
        TreeEntry::Blob { id, size, .. } => {
            let path = path.expect("the root is a directory");
            let content = Content::read(&web, id, size).await?;
            Ok(html::file(&repo, loc.at(), &path, size, &content))
        }
    }
}

/// A page of the first-parent history at a revision, of the whole
/// repository or of one path.
async fn log(
    State(web): State<Web>,
    Path(repo): Path<String>,
    Query(loc): Query<Loc>,
    headers: HeaderMap,
) -> Page {
    let reader = reader(&web, &headers, &repo).await?;
    let path = loc.path()?;
    let head = resolve(&web, &repo, loc.at()).await?;
    let deriver = Deriver::new(&repo, &*web.meta, &*web.objects);
    // One more than a page, so the page knows where the next one starts.
    let ids = match &path {
        None => scion_core::log(&deriver, head, LOG_PAGE + 1).await?,
        Some(p) => {
            reader.check(&repo, p)?;
            scion_core::path_log(&deriver, head, p, LOG_PAGE + 1).await?
        }
    };
    let mut entries = changesets(&web, &repo, &ids).await?;
    let next = (entries.len() > LOG_PAGE).then(|| {
        entries.truncate(LOG_PAGE);
        ids[LOG_PAGE]
    });
    Ok(html::log(&repo, loc.at(), path.as_ref(), &entries, next))
}

/// Who last changed each line of a file, at a revision.
async fn blame(
    State(web): State<Web>,
    Path(repo): Path<String>,
    Query(loc): Query<Loc>,
    headers: HeaderMap,
) -> Page {
    let reader = reader(&web, &headers, &repo).await?;
    let path = loc
        .path()?
        .ok_or_else(|| bad_request("blame needs a file path".into()))?;
    reader.check(&repo, &path)?;
    let cs = resolve(&web, &repo, loc.at()).await?;
    let deriver = Deriver::new(&repo, &*web.meta, &*web.objects);
    let (runs, root) = futures_util::try_join!(
        Blame::for_path(&deriver, cs, &path),
        deriver.derive::<RootTree>(cs),
    )?;
    let runs = runs.ok_or_else(|| not_found(format!("no file {path} at {}", loc.at())))?;
    let Some(TreeEntry::Blob { id, size, .. }) = root.lookup(&*web.objects, &path).await? else {
        return Err(not_found(format!("no file {path} at {}", loc.at())));
    };
    let content = Content::read(&web, id, size).await?;
    Ok(html::blame(&repo, loc.at(), &path, &runs.runs, &content))
}

/// One changeset and its diff against its first parent.
async fn changeset(
    State(web): State<Web>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Page {
    let reader = reader(&web, &headers, &repo).await?;
    let id = changeset_id(&id)?;
    let cs = web
        .meta
        .get_changeset(&repo, id)
        .await?
        .ok_or_else(|| not_found(format!("no changeset {id} in {repo:?}")))?;
    let deriver = Deriver::new(&repo, &*web.meta, &*web.objects);
    let entries: Vec<scion_core::DiffEntry> =
        scion_core::diff(&deriver, cs.parents.first().copied(), id)
            .await?
            .into_iter()
            .filter(|e| reader.allows(e.path()))
            .collect();
    let diffs: Vec<Option<String>> = stream::iter(entries.iter().take(MAX_DIFFED_FILES).map(sides))
        .map(|files| line_diff(&web, files))
        .buffered(READ_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(html::changeset(&repo, id, &cs, &entries, &diffs))
}

/// The old and new file of a diff entry.
fn sides(
    entry: &scion_core::DiffEntry,
) -> (Option<scion_core::FileEntry>, Option<scion_core::FileEntry>) {
    use scion_core::DiffEntry as D;
    match entry {
        D::Added { new, .. } => (None, Some(*new)),
        D::Deleted { old, .. } => (Some(*old), None),
        D::Modified { old, new, .. } => (Some(*old), Some(*new)),
    }
}

/// The unified diff from `old` to `new`, if both are text that a page shows.
async fn line_diff(
    web: &Web,
    (old, new): (Option<scion_core::FileEntry>, Option<scion_core::FileEntry>),
) -> Result<Option<String>, Error> {
    let side = |f: Option<scion_core::FileEntry>| async move {
        match f {
            None => Ok(Content::Text(String::new())),
            Some(f) => Content::read(web, f.id, f.size).await,
        }
    };
    let (old, new) = futures_util::try_join!(side(old), side(new))?;
    let (Content::Text(old), Content::Text(new)) = (old, new) else {
        return Ok(None);
    };
    let patch = diffy::create_patch(&old, &new).to_string();
    // Drop diffy's `---`/`+++` header: the page names the file.
    Ok(Some(
        patch
            .split_once("\n@@")
            .map(|(_, hunks)| format!("@@{hunks}"))
            .unwrap_or_default(),
    ))
}

/// The newest change requests.
async fn requests(State(web): State<Web>, Path(repo): Path<String>, headers: HeaderMap) -> Page {
    reader(&web, &headers, &repo).await?;
    let requests = web
        .meta
        .list_change_requests(&repo, None, REQUESTS_PAGE)
        .await?;
    Ok(html::requests(&repo, &requests))
}

/// One change request: where it is, and its head, base and landed changeset.
async fn request(
    State(web): State<Web>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Page {
    reader(&web, &headers, &repo).await?;
    let id: u64 = id
        .parse()
        .map_err(|_| bad_request(format!("change request {id:?}")))?;
    let cr = web
        .meta
        .get_change_request(&repo, id)
        .await?
        .ok_or_else(|| not_found(format!("no change request {id} in {repo:?}")))?;
    Ok(html::request(&repo, &cr))
}
