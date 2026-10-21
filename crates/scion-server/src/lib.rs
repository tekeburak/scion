//! `scion-server`: the authoritative tier. It serves the object plane
//! (`ObjectService`) from the object store and the control plane
//! (`ControlService`) from the metadata store plus derived data. It holds no
//! repository copy: every request is answered from Postgres and the object
//! store.

#![forbid(unsafe_code)]

mod api;
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

type Failure = Box<dyn std::error::Error + Send + Sync>;

/// [`serve`] on `listener`, and the web interface with its HTTP API
/// (`/api/v1`) on `web`, until either fails. Both run on one control
/// server, so each repository with requests keeps a single land worker.
/// The pages take the same tokens, signed with `key`; browsers sign in
/// through `github` when it has its client secret.
pub async fn serve_with_web(
    listener: TcpListener,
    web: TcpListener,
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    key: TokenKey,
    github: Option<GitHub>,
) -> Result<(), Failure> {
    let (control, object) = servers(meta.clone(), objects.clone()).await?;
    let api = api::router(meta.clone(), control.clone(), object.clone(), key.clone());
    let login = LoginServer::new(github.clone(), key.clone());
    let pages = {
        let key = key.clone();
        let verify = move |token: &str| {
            key.verify(token, std::time::SystemTime::now())
                .map(|(principal, _)| principal)
                .map_err(|e| e.to_string())
        };
        scion_web::serve(web, meta, objects, Arc::new(verify), login.web_login(), api)
    };
    let grpc = tonic::transport::Server::builder()
        .timeout(RPC_DEADLINE)
        .add_routes(routes_of(control, object, login, key).await)
        .serve_with_incoming(TcpIncoming::from(listener).with_nodelay(Some(true)));
    tokio::try_join!(async { grpc.await.map_err(Failure::from) }, async {
        pages.await.map_err(Failure::from)
    },)?;
    Ok(())
}

/// The control and object servers, after starting the land worker of every
/// repository with requests waiting.
async fn servers(
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
) -> Result<(Arc<ControlServer>, ObjectServer), StoreError> {
    let workers = Arc::new(LandWorkers::new(meta.clone(), objects.clone()));
    for repo in meta.repos_with_open_change_requests().await? {
        workers.wake(&repo);
    }
    let control = Arc::new(ControlServer::new(meta.clone(), objects.clone(), workers));
    Ok((control, ObjectServer::new(meta, objects)))
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
    let (control, object) = servers(meta, objects).await?;
    Ok(routes_of(control, object, LoginServer::new(github, key.clone()), key).await)
}

async fn routes_of(
    control: Arc<ControlServer>,
    object: ObjectServer,
    login: LoginServer,
    key: TokenKey,
) -> Routes {
    let (health, health_service) = tonic_health::server::health_reporter();
    health
        .set_serving::<ObjectServiceServer<ObjectServer>>()
        .await;
    health
        .set_serving::<ControlServiceServer<ControlServer>>()
        .await;
    let control_service = ControlServiceServer::from_arc(control)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let object_service = ObjectServiceServer::new(object)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let login_service = LoginServiceServer::new(login);
    let authenticate = Authenticate(key);
    Routes::new(health_service)
        .add_service(login_service)
        .add_service(InterceptedService::new(
            control_service,
            authenticate.clone(),
        ))
        .add_service(InterceptedService::new(object_service, authenticate))
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
