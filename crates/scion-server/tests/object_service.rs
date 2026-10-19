//! `ObjectService` and health over a real transport, backed by an in-memory
//! object store.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use scion_derived::test_util::REPO;
use scion_proto::v1::{GetBlobRequest, GetTreeRequest, PutBlobRequest, PutTreeRequest};
use scion_store::{MemoryStore, ObjectStorage};
use scion_types::{BlobId, Decode, Encode, FileType, PathComponent, Tree, TreeEntry, TreeId};
use tonic::Code;
use tonic::transport::Channel;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;

async fn start() -> Channel {
    common::serve(
        Arc::new(MemoryStore::with_repo(REPO)),
        Arc::new(ObjectStorage::in_memory()),
    )
    .await
}

#[tokio::test]
async fn blobs_and_trees_round_trip() {
    let mut client = common::objects(start().await);

    let id = client
        .put_blob(PutBlobRequest {
            repo: REPO.into(),
            data: b"fn main() {}\n".to_vec(),
        })
        .await
        .unwrap()
        .into_inner()
        .id;
    assert_eq!(id, BlobId::hash(b"fn main() {}\n").as_bytes());
    let data = client
        .get_blob(GetBlobRequest {
            id: id.clone(),
            repo: REPO.into(),
        })
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
            repo: REPO.into(),
            tree: tree.encode_to_vec(),
        })
        .await
        .unwrap()
        .into_inner()
        .id;
    assert_eq!(tree_id, tree.id().as_bytes());
    let bytes = client
        .get_tree(GetTreeRequest {
            id: tree_id,
            repo: REPO.into(),
        })
        .await
        .unwrap()
        .into_inner()
        .tree;
    assert_eq!(Tree::decode(&bytes).unwrap(), tree);
}

#[tokio::test]
async fn missing_objects_are_not_found() {
    let mut client = common::objects(start().await);
    let id = TreeId::hash(b"absent").as_bytes().to_vec();
    let err = client
        .get_tree(GetTreeRequest {
            id: id.clone(),
            repo: REPO.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    let err = client
        .get_blob(GetBlobRequest {
            id,
            repo: REPO.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn malformed_input_is_rejected() {
    let mut client = common::objects(start().await);
    let err = client
        .get_blob(GetBlobRequest {
            id: vec![1, 2, 3],
            repo: REPO.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let err = client
        .put_tree(PutTreeRequest {
            repo: REPO.into(),
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
