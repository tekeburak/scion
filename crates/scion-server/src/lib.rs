//! `scion-server`: the authoritative tier. It serves the object plane
//! (`ObjectService`) from the object store and the control plane
//! (`ControlService`) from the metadata store plus derived data. It holds no
//! repository copy: every request is answered from Postgres and the object
//! store.

#![forbid(unsafe_code)]

mod backfill;
mod control;
mod land;
mod object;
mod staged;
mod token;

use std::sync::Arc;
use std::time::Duration;

use scion_proto::v1::control_service_server::ControlServiceServer;
use scion_proto::v1::object_service_server::ObjectServiceServer;
use scion_store::{MetadataStore, ObjectStore, StoreError};
use tokio::net::TcpListener;
use tonic::Status;
use tonic::transport::server::TcpIncoming;

use control::ControlServer;
use land::LandWorkers;
use object::ObjectServer;

pub use backfill::{BACKFILL_TYPES, backfill};
pub use control::{
    MAX_LOG_LIMIT, Merged, Rebased, Reverted, create_changesets, merge, move_ref, rebase, revert,
};
pub use token::{TokenError, TokenKey};

/// Upper bound on every RPC. A client may ask for less with `grpc-timeout`;
/// it cannot get more.
pub const RPC_DEADLINE: Duration = Duration::from_secs(30);

/// Largest message either side may send. This is also the per-blob size
/// limit: blobs travel in one message and large-file chunking is a non-goal.
pub const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Serve all services on `listener` until the future is dropped or the
/// transport fails, first starting the land worker of every repository with
/// requests waiting.
pub async fn serve(
    listener: TcpListener,
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
        ControlServiceServer::new(ControlServer::new(meta, objects.clone(), workers))
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let object_service = ObjectServiceServer::new(ObjectServer::new(objects))
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    tonic::transport::Server::builder()
        .timeout(RPC_DEADLINE)
        .add_service(health_service)
        .add_service(control_service)
        .add_service(object_service)
        // Without TCP_NODELAY a small reply waits for the client's delayed
        // ACK, about 40 ms on Linux, whenever two writes go out back to back.
        .serve_with_incoming(TcpIncoming::from(listener).with_nodelay(Some(true)))
        .await?;
    Ok(())
}

/// Map a store failure to a gRPC status.
#[allow(clippy::needless_pass_by_value)] // used as `map_err(store_status)`
fn store_status(e: StoreError) -> Status {
    match e {
        StoreError::Corrupt(_) => Status::data_loss(e.to_string()),
        StoreError::RepoNotFound(_) => Status::not_found(e.to_string()),
        _ => Status::internal(e.to_string()),
    }
}
