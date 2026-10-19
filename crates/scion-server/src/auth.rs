//! Authentication at the boundary: every control and object request carries
//! `authorization: Bearer <token>`, checked here once. The principal it names
//! travels with the request to [`authorize`], which each RPC calls with the
//! role it needs.

use std::time::SystemTime;

use scion_store::{MetadataStore, Role, SERVER_GRANTS};
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

/// Refuse `request` with `PERMISSION_DENIED` unless its principal holds `need`
/// or more in `repo` ([`SERVER_GRANTS`] for server-wide roles). Every grant
/// of the repository counts, whatever its path prefix.
pub(crate) async fn authorize<T>(
    meta: &dyn MetadataStore,
    request: &Request<T>,
    repo: &str,
    need: Role,
) -> Result<(), Status> {
    let principal = principal(request)?;
    let held = meta
        .grants_for(repo, principal)
        .await
        .map_err(store_status)?
        .into_iter()
        .map(|g| g.role)
        .max();
    if held >= Some(need) {
        return Ok(());
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
