//! The generated client and server agree over a real transport.

use scion_proto::v1::object_service_client::ObjectServiceClient;
use scion_proto::v1::object_service_server::{ObjectService, ObjectServiceServer};
use scion_proto::v1::{
    GetBlobRequest, GetBlobResponse, GetTreeRequest, GetTreeResponse, PutBlobRequest,
    PutBlobResponse, PutTreeRequest, PutTreeResponse,
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

/// Echoes blob bytes back as their "id"; enough to see bytes cross the wire.
#[derive(Debug)]
struct Echo;

#[tonic::async_trait]
impl ObjectService for Echo {
    async fn get_tree(
        &self,
        _: Request<GetTreeRequest>,
    ) -> Result<Response<GetTreeResponse>, Status> {
        Err(Status::not_found("no trees"))
    }

    async fn get_blob(
        &self,
        r: Request<GetBlobRequest>,
    ) -> Result<Response<GetBlobResponse>, Status> {
        Ok(Response::new(GetBlobResponse {
            data: r.into_inner().id,
        }))
    }

    async fn put_blob(
        &self,
        r: Request<PutBlobRequest>,
    ) -> Result<Response<PutBlobResponse>, Status> {
        Ok(Response::new(PutBlobResponse {
            id: r.into_inner().data,
        }))
    }

    async fn put_tree(
        &self,
        _: Request<PutTreeRequest>,
    ) -> Result<Response<PutTreeResponse>, Status> {
        Err(Status::unimplemented("read-only"))
    }
}

#[tokio::test]
async fn client_and_server_interoperate() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(ObjectServiceServer::new(Echo))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );

    let mut client = ObjectServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    let id = client
        .put_blob(PutBlobRequest {
            data: b"hello".to_vec(),
        })
        .await
        .unwrap()
        .into_inner()
        .id;
    assert_eq!(id, b"hello");
    let data = client
        .get_blob(GetBlobRequest { id })
        .await
        .unwrap()
        .into_inner()
        .data;
    assert_eq!(data, b"hello");

    let err = client
        .get_tree(GetTreeRequest { id: vec![0; 32] })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}
