//! Authentication at the boundary: every control and object request carries
//! `authorization: Bearer <token>`, checked here once. The principal it names
//! travels with the request to [`authorize`], which each RPC calls with the
//! role it needs; the [`Scope`] it returns limits the paths the request may
//! change or read to the directories of the principal's grants.

use std::time::SystemTime;

use scion_store::{MetadataStore, Role, SERVER_GRANTS};
use scion_types::RepoPath;
use tonic::service::Interceptor;
use tonic::{Request, Status};

use crate::store_status;
use crate::token::TokenKey;

/// The principal a request's token names, put there by [`Authenticate`].
#[derive(Debug, Clone)]
pub(crate) struct Principal(pub(crate) String);

/// Refuses a request whose bearer token is missing, malformed, signed with
/// another key or expired, with UNAUTHENTICATED.
#[derive(Debug, Clone)]
pub(crate) struct Authenticate(pub(crate) TokenKey);

impl Interceptor for Authenticate {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        let Some(value) = req.metadata().get("authorization") else {
            return Err(refused("no token"));
        };
        let token = value
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| refused("authorization is not `Bearer <token>`"))?;
        let principal = self
            .0
            .verify(token.trim(), SystemTime::now())
            .map_err(|e| refused(&e.to_string()))?;
        req.extensions_mut().insert(Principal(principal));
        Ok(req)
    }
}

fn refused(why: &str) -> Status {
    Status::unauthenticated(format!(
        "{why}: sign in with `scn login <server>`, or set SCN_TOKEN to a token from \
         `scion-server token`"
    ))
}

/// The principal of an authenticated request.
pub(crate) fn principal<T>(request: &Request<T>) -> Result<&str, Status> {
    request
        .extensions()
        .get::<Principal>()
        .map(|p| p.0.as_str())
        .ok_or_else(|| refused("no token"))
}

/// The paths a principal holds a role in, in one repository: the prefixes
/// of its grants with that role or more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    principal: String,
    repo: String,
    role: Role,
    /// Directories without a trailing `/`; empty for the whole repository.
    prefixes: Vec<String>,
}

impl Scope {
    /// Every path of every repository, for code that acts for no principal.
    #[must_use]
    pub fn whole() -> Self {
        Self {
            principal: String::new(),
            repo: String::new(),
            role: Role::Admin,
            prefixes: vec![String::new()],
        }
    }

    /// What `principal` holds `role` or more on in `repo`; no path if it
    /// holds nothing there.
    pub(crate) async fn of(
        meta: &dyn MetadataStore,
        repo: &str,
        principal: &str,
        role: Role,
    ) -> Result<Self, Status> {
        let prefixes = meta
            .grants_for(repo, principal)
            .await
            .map_err(store_status)?
            .into_iter()
            .filter(|g| g.role >= role)
            .map(|g| g.path_prefix)
            .collect();
        Ok(Self {
            principal: principal.to_owned(),
            repo: repo.to_owned(),
            role,
            prefixes,
        })
    }

    /// Whether the scope covers the whole repository.
    pub(crate) fn is_whole(&self) -> bool {
        self.prefixes.iter().any(String::is_empty)
    }

    /// Whether `path` is under one of the scope's directories.
    pub(crate) fn allows(&self, path: &RepoPath) -> bool {
        let path = path.as_str();
        self.prefixes.iter().any(|p| {
            p.is_empty()
                || path
                    .strip_prefix(p.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }

    /// The paths of `paths` the scope does not cover, sorted.
    pub(crate) fn denied<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a RepoPath>,
    ) -> Vec<RepoPath> {
        let mut denied: Vec<RepoPath> = paths
            .into_iter()
            .filter(|p| !self.allows(p))
            .cloned()
            .collect();
        denied.sort();
        denied.dedup();
        denied
    }

    /// Refuse with `PERMISSION_DENIED`, naming the first path the scope does
    /// not cover, unless it covers all of `paths`; `doing` is what the
    /// request does to them ("change", "read").
    pub(crate) fn check<'a>(
        &self,
        doing: &str,
        paths: impl IntoIterator<Item = &'a RepoPath>,
    ) -> Result<(), Status> {
        match self.denied(paths).first() {
            None => Ok(()),
            Some(path) => Err(self.refuse(doing, path)),
        }
    }

    /// `PERMISSION_DENIED` for doing `doing` to `path`, outside the scope.
    pub(crate) fn refuse(&self, doing: &str, path: &RepoPath) -> Status {
        let (principal, repo, role) = (&self.principal, &self.repo, self.role);
        let held = if self.prefixes.is_empty() {
            "nothing".to_owned()
        } else {
            self.prefixes
                .iter()
                .map(|p| format!("{p}/"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let at = path
            .parent()
            .map(|dir| format!(" --path {dir}"))
            .unwrap_or_default();
        Status::permission_denied(format!(
            "{principal} may not {doing} {path}: its {role} grants in {repo:?} cover {held}; \
             an admin can run `scion-server grant {repo} {principal} {role}{at}`"
        ))
    }
}

/// Refuse `request` with `PERMISSION_DENIED` unless its principal holds `need`
/// or more in `repo` ([`SERVER_GRANTS`] for server-wide roles) on some path;
/// returns the paths it holds `need` on.
pub(crate) async fn authorize<T>(
    meta: &dyn MetadataStore,
    request: &Request<T>,
    repo: &str,
    need: Role,
) -> Result<Scope, Status> {
    let principal = principal(request)?;
    let scope = Scope::of(meta, repo, principal, need).await?;
    if !scope.prefixes.is_empty() {
        return Ok(scope);
    }
    let quoted = if repo == SERVER_GRANTS { "'*'" } else { repo };
    let what = if repo == SERVER_GRANTS {
        format!("{principal} is not a server {need}")
    } else {
        format!("{principal} is not a {need} of {repo:?}")
    };
    Err(Status::permission_denied(format!(
        "{what}; an admin can run `scion-server grant {quoted} {principal} {need}`"
    )))
}
