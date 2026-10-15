//! `ObjectService` and health over a real transport, backed by an in-memory
//! object store.

use std::collections::BTreeMap;
use std::sync::Arc;

use scion_proto::v1::object_service_client::ObjectServiceClient;
use scion_proto::v1::{GetBlobRequest, GetTreeRequest, PutBlobRequest, PutTreeRequest};
use scion_store::{MemoryStore, ObjectStorage};
use scion_types::{BlobId, Decode, Encode, FileType, PathComponent, Tree, TreeEntry, TreeId};
use tokio::net::TcpListener;
use tonic::Code;
use tonic::transport::Channel;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;

async fn start() -> Channel {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(scion_server::serve(
        listener,
        Arc::new(MemoryStore::new()),
        Arc::new(ObjectStorage::in_memory()),
    ));
    Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

#[tokio::test]
async fn blobs_and_trees_round_trip() {
    let mut client = ObjectServiceClient::new(start().await);

    let id = client
        .put_blob(PutBlobRequest {
            data: b"fn main() {}\n".to_vec(),
        })
        .await
        .unwrap()
        .into_inner()
        .id;
    assert_eq!(id, BlobId::hash(b"fn main() {}\n").as_bytes());
    let data = client
        .get_blob(GetBlobRequest { id: id.clone() })
        .await
        .unwrap()
        .into_inner()
        .data;
    assert_eq!(data, b"fn main() {}\n");

    let tree = Tree {
        entries: BTreeMap::from([(
            PathComponent::new("main.rs").unwrap(),
            TreeEntry::Blob {
                id: BlobId::try_from_slice(&id).unwrap(),
                kind: FileType::Regular,
                size: 13,
            },
        )]),
    };
    let tree_id = client
        .put_tree(PutTreeRequest {
            tree: tree.encode_to_vec(),
        })
        .await
        .unwrap()
        .into_inner()
        .id;
    assert_eq!(tree_id, tree.id().as_bytes());
    let bytes = client
        .get_tree(GetTreeRequest { id: tree_id })
        .await
        .unwrap()
        .into_inner()
        .tree;
    assert_eq!(Tree::decode(&bytes).unwrap(), tree);
}

#[tokio::test]
async fn missing_objects_are_not_found() {
    let mut client = ObjectServiceClient::new(start().await);
    let id = TreeId::hash(b"absent").as_bytes().to_vec();
    let err = client
        .get_tree(GetTreeRequest { id: id.clone() })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    let err = client.get_blob(GetBlobRequest { id }).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn malformed_input_is_rejected() {
    let mut client = ObjectServiceClient::new(start().await);
    let err = client
        .get_blob(GetBlobRequest { id: vec![1, 2, 3] })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let err = client
        .put_tree(PutTreeRequest {
            tree: b"not a tree".to_vec(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn health_reports_serving() {
    let mut health = HealthClient::new(start().await);
    for service in ["", "scion.v1.ObjectService", "scion.v1.ControlService"] {
        let status = health
            .check(HealthCheckRequest {
                service: service.to_owned(),
            })
            .await
            .unwrap()
            .into_inner()
            .status();
        assert_eq!(status, ServingStatus::Serving, "service {service:?}");
    }
}
