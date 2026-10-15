//! `scion-server`: the authoritative tier. It serves the object plane
//! (`ObjectService`) from the object store and, from Phase 3.3 on, the control
//! plane from the metadata store. It holds no repository copy: every request
//! is answered from Postgres and the object store.

#![forbid(unsafe_code)]

mod object;

use std::sync::Arc;
use std::time::Duration;

use scion_proto::v1::object_service_server::ObjectServiceServer;
use scion_store::{ObjectStore, StoreError};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Status;

use object::ObjectServer;

/// Upper bound on every RPC. A client may ask for less with `grpc-timeout`;
/// it cannot get more.
pub const RPC_DEADLINE: Duration = Duration::from_secs(30);

/// Largest message either side may send. This is also the per-blob size
/// limit: blobs travel in one message and large-file chunking is a non-goal.
pub const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Serve all services on `listener` until the future is dropped or the
/// transport fails.
pub async fn serve(
    listener: TcpListener,
    objects: Arc<dyn ObjectStore>,
) -> Result<(), tonic::transport::Error> {
    let (health, health_service) = tonic_health::server::health_reporter();
    health
        .set_serving::<ObjectServiceServer<ObjectServer>>()
        .await;
    let object_service = ObjectServiceServer::new(ObjectServer::new(objects))
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    tonic::transport::Server::builder()
        .timeout(RPC_DEADLINE)
        .add_service(health_service)
        .add_service(object_service)
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await
}

/// Map a store failure to a gRPC status.
#[allow(clippy::needless_pass_by_value)] // used as `map_err(store_status)`
fn store_status(e: StoreError) -> Status {
    match e {
        StoreError::Corrupt(_) => Status::data_loss(e.to_string()),
        _ => Status::internal(e.to_string()),
    }
}
