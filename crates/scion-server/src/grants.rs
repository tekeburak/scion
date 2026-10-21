//! Managing access from a client: an admin of a whole repository grants,
//! revokes and lists its roles, and any caller asks who its token names.
//! What the client sends is checked as `scion-server grant` checks its
//! command line.

use std::time::UNIX_EPOCH;

use scion_proto::v1::{
    AccessGrant, GrantRequest, GrantResponse, ListGrantsRequest, ListGrantsResponse, RevokeRequest,
    RevokeResponse, WhoAmIRequest, WhoAmIResponse,
};
use scion_store::{Grant, MetadataStore, Role};
use tonic::{Request, Response, Status};

use crate::auth::{authorize, principal, token_of};
use crate::operator::{grantee_arg, parse_grant, parse_prefix};
use crate::store_status;

/// Refuse `request` unless its principal is an admin of all of `repo`.
async fn whole_admin<T>(
    meta: &dyn MetadataStore,
    request: &Request<T>,
    repo: &str,
) -> Result<(), Status> {
    if authorize(meta, request, repo, Role::Admin)
        .await?
        .is_whole()
    {
        return Ok(());
    }
    Err(Status::permission_denied(format!(
        "{} is an admin of directories of {repo:?} only; managing access needs the whole \
         repository",
        principal(request)?
    )))
}

fn access_grant(g: Grant) -> AccessGrant {
    AccessGrant {
        principal: g.principal,
        path_prefix: g.path_prefix,
        role: g.role.to_string(),
    }
}

/// `None` for the whole repository, as the wire spells it: empty.
fn dir(path_prefix: &str) -> Option<&str> {
    Some(path_prefix).filter(|p| !p.is_empty())
}

pub(crate) async fn grant(
    meta: &dyn MetadataStore,
    request: Request<GrantRequest>,
) -> Result<Response<GrantResponse>, Status> {
    let repo = &request.get_ref().repo;
    whole_admin(meta, &request, repo).await?;
    let g = request.get_ref().grant.clone().unwrap_or_default();
    let grant = parse_grant(repo, &g.principal, &g.role, dir(&g.path_prefix))
        .map_err(Status::invalid_argument)?;
    meta.put_grant(repo, &grant).await.map_err(store_status)?;
    Ok(Response::new(GrantResponse {}))
}

pub(crate) async fn revoke(
    meta: &dyn MetadataStore,
    request: Request<RevokeRequest>,
) -> Result<Response<RevokeResponse>, Status> {
    let req = request.get_ref();
    whole_admin(meta, &request, &req.repo).await?;
    let principal = grantee_arg(&req.principal).map_err(Status::invalid_argument)?;
    let prefix =
        parse_prefix(&req.repo, dir(&req.path_prefix)).map_err(Status::invalid_argument)?;
    if meta
        .delete_grant(&req.repo, &principal, &prefix)
        .await
        .map_err(store_status)?
    {
        return Ok(Response::new(RevokeResponse {}));
    }
    let on = if prefix.is_empty() {
        "the whole repository".to_owned()
    } else {
        format!("{prefix}/")
    };
    Err(Status::not_found(format!(
        "{principal} has no grant on {on} in {:?}",
        req.repo
    )))
}

pub(crate) async fn list_grants(
    meta: &dyn MetadataStore,
    request: Request<ListGrantsRequest>,
) -> Result<Response<ListGrantsResponse>, Status> {
    let repo = &request.get_ref().repo;
    whole_admin(meta, &request, repo).await?;
    let grants = meta.grants(repo).await.map_err(store_status)?;
    Ok(Response::new(ListGrantsResponse {
        grants: grants.into_iter().map(access_grant).collect(),
    }))
}

pub(crate) async fn who_am_i(
    meta: &dyn MetadataStore,
    request: Request<WhoAmIRequest>,
) -> Result<Response<WhoAmIResponse>, Status> {
    let token = token_of(&request)?;
    let repo = &request.get_ref().repo;
    let grants = if repo.is_empty() {
        Vec::new()
    } else {
        meta.grants_for(repo, &token.0)
            .await
            .map_err(store_status)?
    };
    Ok(Response::new(WhoAmIResponse {
        principal: token.0.clone(),
        expires_unix_seconds: token
            .1
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
        grants: grants.into_iter().map(access_grant).collect(),
    }))
}
