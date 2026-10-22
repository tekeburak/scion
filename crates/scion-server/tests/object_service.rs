//! `ObjectService` and health over a real transport, backed by an in-memory
//! object store.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use scion_derived::test_util::REPO;
use scion_proto::v1::{
    GetBlobRequest, GetObjectsRequest, GetTreeRequest, HasBlobsRequest, PutBlobRequest,
    PutBlobsRequest, PutTreeRequest, object,
};
use scion_store::{MemoryStore, ObjectStorage};
use scion_types::{
    BlobId, Decode, Encode, FileType, MAX_BLOB_BYTES, PathComponent, Tree, TreeEntry, TreeId,
};
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

/// A tree holding `entries`, stored through `client`.
async fn put_tree(client: &mut common::Objects, entries: Vec<(&str, TreeEntry)>) -> TreeId {
    let tree = Tree {
        entries: entries
            .into_iter()
            .map(|(name, e)| (PathComponent::new(name).unwrap(), e))
            .collect(),
    };
    client
        .put_tree(PutTreeRequest {
            repo: REPO.into(),
            tree: tree.encode_to_vec(),
        })
        .await
        .unwrap();
    tree.id()
}

/// Every object of a `GetObjects` reply, by id.
async fn get_objects(
    client: &mut common::Objects,
    trees: &[TreeId],
    subtrees: &[TreeId],
    blobs: &[BlobId],
) -> Result<Vec<(Vec<u8>, object::Object)>, tonic::Status> {
    let mut reply = client
        .get_objects(GetObjectsRequest {
            repo: REPO.into(),
            trees: trees.iter().map(|id| id.as_bytes().to_vec()).collect(),
            subtrees: subtrees.iter().map(|id| id.as_bytes().to_vec()).collect(),
            blobs: blobs.iter().map(|id| id.as_bytes().to_vec()).collect(),
        })
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(o) = reply.message().await? {
        out.push((o.id, o.object.unwrap()));
    }
    Ok(out)
}

#[tokio::test]
async fn get_objects_streams_trees_subtrees_and_blobs() {
    let mut client = common::objects(start().await);
    let blob = BlobId::hash(b"x\n");
    client
        .put_blob(PutBlobRequest {
            repo: REPO.into(),
            data: b"x\n".to_vec(),
        })
        .await
        .unwrap();
    let file = TreeEntry::Blob {
        id: blob,
        kind: FileType::Regular,
        size: 2,
    };
    // `leaf` sits beneath `root` twice; it comes once, after its parents.
    let leaf = put_tree(&mut client, vec![("f", file.clone())]).await;
    let mid = put_tree(&mut client, vec![("leaf", TreeEntry::Tree { id: leaf })]).await;
    let root = put_tree(
        &mut client,
        vec![
            ("a", TreeEntry::Tree { id: mid }),
            ("b", TreeEntry::Tree { id: leaf }),
        ],
    )
    .await;

    let got = get_objects(&mut client, &[], &[root], &[]).await.unwrap();
    let ids: Vec<_> = got.iter().map(|(id, _)| id.clone()).collect();
    let pos = |id: TreeId| ids.iter().position(|i| i == id.as_bytes()).unwrap();
    assert_eq!(ids.len(), 3, "each tree once");
    assert!(pos(root) < pos(mid) && pos(root) < pos(leaf));
    for (id, o) in &got {
        let object::Object::Tree(bytes) = o else {
            panic!("a blob in a tree reply")
        };
        assert_eq!(Tree::decode(bytes).unwrap().id().as_bytes(), &id[..]);
    }

    let got = get_objects(&mut client, &[root], &[], &[blob])
        .await
        .unwrap();
    assert_eq!(got.len(), 2, "a plain tree comes without its subtrees");
    assert!(got.contains(&(
        blob.as_bytes().to_vec(),
        object::Object::Blob(b"x\n".to_vec())
    )));

    let absent = BlobId::hash(b"absent");
    let err = get_objects(&mut client, &[], &[], &[blob, absent])
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    let err = get_objects(&mut client, &[TreeId::hash(b"absent")], &[], &[])
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    let err = get_objects(&mut client, &[], &[], &vec![blob; 257])
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn put_blobs_then_has_blobs() {
    let mut client = common::objects(start().await);
    let blobs: Vec<Vec<u8>> = (0..300).map(|i| format!("{i}\n").into_bytes()).collect();
    let ids: Vec<Vec<u8>> = blobs
        .iter()
        .map(|b| BlobId::hash(b).as_bytes().to_vec())
        .collect();
    let messages = blobs.iter().take(200).map(|data| PutBlobsRequest {
        repo: REPO.into(),
        data: data.clone(),
    });
    client
        .put_blobs(futures_util::stream::iter(messages.collect::<Vec<_>>()))
        .await
        .unwrap();
    let present = client
        .has_blobs(HasBlobsRequest {
            repo: REPO.into(),
            ids: ids[150..250].to_vec(),
        })
        .await
        .unwrap()
        .into_inner()
        .present;
    assert_eq!(present, (150..250).map(|i| i < 200).collect::<Vec<_>>());
    let err = client
        .has_blobs(HasBlobsRequest {
            repo: REPO.into(),
            ids: ids[..257].to_vec(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);

    // Every message names the first one's repository; a blob over the limit
    // is refused by name.
    let mixed = vec![
        PutBlobsRequest {
            repo: REPO.into(),
            data: b"a".to_vec(),
        },
        PutBlobsRequest {
            repo: "other".into(),
            data: b"b".to_vec(),
        },
    ];
    let err = client
        .put_blobs(futures_util::stream::iter(mixed))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let over = vec![PutBlobsRequest {
        repo: REPO.into(),
        data: vec![b'a'; MAX_BLOB_BYTES + 1],
    }];
    let err = client
        .put_blobs(futures_util::stream::iter(over))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted);
    assert!(
        err.message().contains("per-blob limit"),
        "{}",
        err.message()
    );
}
