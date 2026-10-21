//! `scion-server`: the authoritative tier. It serves the object plane
//! (`ObjectService`) from the object store and the control plane
//! (`ControlService`) from the metadata store plus derived data. It holds no
//! repository copy: every request is answered from Postgres and the object
//! store.

#![forbid(unsafe_code)]

mod auth;
mod backfill;
mod control;
mod grants;
mod land;
mod login;
mod object;
mod operator;
mod staged;
mod token;

use std::sync::Arc;
use std::time::Duration;

use scion_proto::v1::control_service_server::ControlServiceServer;
use scion_proto::v1::login_service_server::LoginServiceServer;
use scion_proto::v1::object_service_server::ObjectServiceServer;
use scion_store::{MetadataStore, ObjectStore, StoreError};
use tokio::net::TcpListener;
use tonic::Status;
use tonic::service::Routes;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::server::TcpIncoming;

use auth::Authenticate;
use control::ControlServer;
use land::LandWorkers;
use login::LoginServer;
use object::ObjectServer;

pub use auth::Scope;
pub use backfill::{BACKFILL_TYPES, backfill};
pub use control::{
    MAX_LOG_LIMIT, Merged, Rebased, Reverted, create_changesets, merge, move_ref, rebase, revert,
};
pub use login::{GitHub, LOGIN_TTL};
pub use operator::{Command, USAGE, grant, revoke, token};
pub use token::{TokenError, TokenKey};

/// Upper bound on every RPC. A client may ask for less with `grpc-timeout`;
/// it cannot get more.
pub const RPC_DEADLINE: Duration = Duration::from_secs(30);

/// Largest message either side may send. This is also the per-blob size
/// limit: blobs travel in one message and large-file chunking is a non-goal.
pub const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Serve all services on `listener` until the future is dropped or the
/// transport fails, first starting the land worker of every repository with
/// requests waiting. Control and object requests need a token signed with
/// `key`; `scn login` gets one through `github`, if given.
pub async fn serve(
    listener: TcpListener,
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    key: TokenKey,
    github: Option<GitHub>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tonic::transport::Server::builder()
        .timeout(RPC_DEADLINE)
        .add_routes(routes(meta, objects, key, github).await?)
        // Without TCP_NODELAY a small reply waits for the client's delayed
        // ACK, about 40 ms on Linux, whenever two writes go out back to back.
        .serve_with_incoming(TcpIncoming::from(listener).with_nodelay(Some(true)))
        .await?;
    Ok(())
}

/// Serve the web interface on `listener` until the future is dropped or
/// accepting fails. Its pages read the same stores as the control plane and
/// take the same tokens, signed with `key`; browsers sign in through
/// `github` when it has its client secret.
pub async fn serve_web(
    listener: TcpListener,
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    key: TokenKey,
    github: Option<GitHub>,
) -> std::io::Result<()> {
    let login = LoginServer::new(github, key.clone()).web_login();
    let verify = move |token: &str| {
        key.verify(token, std::time::SystemTime::now())
            .map(|(principal, _)| principal)
            .map_err(|e| e.to_string())
    };
    scion_web::serve(listener, meta, objects, Arc::new(verify), login).await
}

/// The health, login, control and object services [`serve`] runs, for a
/// server built with its own layers. Starts the land worker of every
/// repository with requests waiting. Control and object requests need a
/// token signed with `key`; health checks and login need none, and login
/// answers `FAILED_PRECONDITION` without `github`.
pub async fn routes(
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    key: TokenKey,
    github: Option<GitHub>,
) -> Result<Routes, StoreError> {
    let workers = Arc::new(LandWorkers::new(meta.clone(), objects.clone()));
    for repo in meta.repos_with_open_change_requests().await? {
        workers.wake(&repo);
    }
    let (health, health_service) = tonic_health::server::health_reporter();
    health
        .set_serving::<ObjectServiceServer<ObjectServer>>()
        .await;
    health
        .set_serving::<ControlServiceServer<ControlServer>>()
        .await;
    let control_service =
        ControlServiceServer::new(ControlServer::new(meta.clone(), objects.clone(), workers))
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let object_service = ObjectServiceServer::new(ObjectServer::new(meta.clone(), objects))
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let login_service = LoginServiceServer::new(LoginServer::new(github, key.clone()));
    let authenticate = Authenticate(key);
    Ok(Routes::new(health_service)
        .add_service(login_service)
        .add_service(InterceptedService::new(
            control_service,
            authenticate.clone(),
        ))
        .add_service(InterceptedService::new(object_service, authenticate)))
}

/// Map a store failure to a gRPC status.
#[allow(clippy::needless_pass_by_value)] // used as `map_err(store_status)`
fn store_status(e: StoreError) -> Status {
    match e {
        StoreError::Corrupt(_) => Status::data_loss(e.to_string()),
        StoreError::RepoNotFound(_) => Status::not_found(e.to_string()),
        StoreError::ReservedRepoName(_) => Status::invalid_argument(e.to_string()),
        _ => Status::internal(e.to_string()),
    }
}
