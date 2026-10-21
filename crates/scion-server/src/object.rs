//! `ObjectService` over an [`ObjectStore`]. Requests come from clients, so ids
//! and tree bytes are validated here; the store verifies what it reads back.
//! Every request needs `reader` in the repository it names.

use std::sync::Arc;

use bytes::Bytes;
use scion_proto::trace::{span, timed};
use scion_proto::v1::object_service_server::ObjectService;
use scion_proto::v1::{
    GetBlobRequest, GetBlobResponse, GetTreeRequest, GetTreeResponse, PutBlobRequest,
    PutBlobResponse, PutTreeRequest, PutTreeResponse,
};
use scion_store::{MetadataStore, ObjectStore, Role};
use scion_types::{BlobId, Decode, Encode, Tree, TreeId};
use tonic::{Request, Response, Status};

use crate::auth::authorize;
use crate::store_status;

/// Serves trees and blobs by id from an object store.
#[derive(Clone)]
pub(crate) struct ObjectServer {
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
}

impl std::fmt::Debug for ObjectServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectServer").finish_non_exhaustive()
    }
}

impl ObjectServer {
    /// Serve objects from `objects`, to principals with grants in `meta`.
    pub(crate) fn new(meta: Arc<dyn MetadataStore>, objects: Arc<dyn ObjectStore>) -> Self {
        Self { meta, objects }
    }
}

fn tree_id(bytes: &[u8]) -> Result<TreeId, Status> {
    TreeId::try_from_slice(bytes).map_err(|e| Status::invalid_argument(format!("tree id: {e}")))
}

fn blob_id(bytes: &[u8]) -> Result<BlobId, Status> {
    BlobId::try_from_slice(bytes).map_err(|e| Status::invalid_argument(format!("blob id: {e}")))
}

#[tonic::async_trait]
impl ObjectService for ObjectServer {
    async fn get_tree(
        &self,
        request: Request<GetTreeRequest>,
    ) -> Result<Response<GetTreeResponse>, Status> {
        let _rpc = span("GetTree");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
        let id = tree_id(&request.get_ref().id)?;
        let tree = self
            .objects
            .get_tree(id)
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::not_found(format!("tree {id}")))?;
        Ok(Response::new(GetTreeResponse {
            tree: tree.encode_to_vec(),
        }))
    }

    async fn get_blob(
        &self,
        request: Request<GetBlobRequest>,
    ) -> Result<Response<GetBlobResponse>, Status> {
        let _rpc = span("GetBlob");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
        let id = blob_id(&request.get_ref().id)?;
        let data = self
            .objects
            .get_blob(id)
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::not_found(format!("blob {id}")))?;
        Ok(Response::new(GetBlobResponse {
            data: data.to_vec(),
        }))
    }

    async fn put_blob(
        &self,
        request: Request<PutBlobRequest>,
    ) -> Result<Response<PutBlobResponse>, Status> {
        let _rpc = span("PutBlob");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
        let data = Bytes::from(request.into_inner().data);
        let id = self.objects.put_blob(data).await.map_err(store_status)?;
        Ok(Response::new(PutBlobResponse {
            id: id.as_bytes().to_vec(),
        }))
    }

    async fn put_tree(
        &self,
        request: Request<PutTreeRequest>,
    ) -> Result<Response<PutTreeResponse>, Status> {
        let _rpc = span("PutTree");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
        let tree = Tree::decode(&request.get_ref().tree)
            .map_err(|e| Status::invalid_argument(format!("tree: {e}")))?;
        let id = self.objects.put_tree(&tree).await.map_err(store_status)?;
        Ok(Response::new(PutTreeResponse {
            id: id.as_bytes().to_vec(),
        }))
    }
}
