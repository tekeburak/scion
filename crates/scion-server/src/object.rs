//! `ObjectService` over an [`ObjectStore`]. Requests come from clients, so ids
//! and tree bytes are validated here; the store verifies what it reads back.
//! Every request needs `reader` in the repository it names; a blob over
//! [`MAX_BLOB_BYTES`] is refused with `RESOURCE_EXHAUSTED`.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt, stream};
use scion_proto::MAX_IDS;
use scion_proto::trace::{span, timed};
use scion_proto::v1::object_service_server::ObjectService;
use scion_proto::v1::{
    GetBlobRequest, GetBlobResponse, GetObjectsRequest, GetTreeRequest, GetTreeResponse,
    HasBlobsRequest, HasBlobsResponse, Object, PutBlobRequest, PutBlobResponse, PutBlobsRequest,
    PutBlobsResponse, PutTreeRequest, PutTreeResponse, object,
};
use scion_store::{MetadataStore, ObjectStore, Role};
use scion_types::{
    BlobId, Decode, Encode, MAX_BLOB_BYTES, Tree, TreeEntry, TreeId, blob_too_large,
};
use tonic::{Request, Response, Status, Streaming};

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

/// Most object-store reads or writes one `GetObjects` or `PutBlobs` call has
/// in flight.
const OBJECT_CONCURRENCY: usize = 32;

fn too_many_ids(n: usize) -> Status {
    Status::invalid_argument(format!("{n} ids; at most {MAX_IDS} per request"))
}

/// The objects a `GetObjects` call streams back.
type Objects = Pin<Box<dyn Stream<Item = Result<Object, Status>> + Send>>;

fn tree_object(id: TreeId, tree: &Tree) -> Object {
    Object {
        id: id.as_bytes().to_vec(),
        object: Some(object::Object::Tree(tree.encode_to_vec())),
    }
}

/// `repo`'s trees `ids`, all of which must exist.
async fn trees(
    meta: &dyn MetadataStore,
    repo: &str,
    ids: &[TreeId],
) -> Result<Vec<(TreeId, Tree)>, Status> {
    let found = meta.get_trees(repo, ids).await.map_err(store_status)?;
    ids.iter()
        .zip(found)
        .map(|(&id, tree)| {
            tree.map(|t| (id, t))
                .ok_or_else(|| Status::not_found(format!("tree {id}")))
        })
        .collect()
}

/// `roots` and every tree beneath them, one level per metadata read, each id
/// once.
fn subtrees(meta: Arc<dyn MetadataStore>, repo: String, roots: Vec<TreeId>) -> Objects {
    let seen: HashSet<TreeId> = roots.iter().copied().collect();
    let levels = stream::try_unfold((roots, seen), move |(level, mut seen)| {
        let meta = meta.clone();
        let repo = repo.clone();
        async move {
            if level.is_empty() {
                return Ok::<_, Status>(None);
            }
            let found = trees(&*meta, &repo, &level).await?;
            let mut next = Vec::new();
            for (_, tree) in &found {
                for entry in tree.entries.values() {
                    if let TreeEntry::Tree { id } = entry {
                        if seen.insert(*id) {
                            next.push(*id);
                        }
                    }
                }
            }
            let objects: Vec<_> = found
                .iter()
                .map(|(id, t)| Ok(tree_object(*id, t)))
                .collect();
            Ok(Some((stream::iter(objects), (next, seen))))
        }
    });
    Box::pin(levels.try_flatten())
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
            .meta
            .get_tree(&request.get_ref().repo, id)
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
        if data.len() > MAX_BLOB_BYTES {
            return Err(Status::resource_exhausted(blob_too_large(data.len())));
        }
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
        let req = request.get_ref();
        let tree =
            Tree::decode(&req.tree).map_err(|e| Status::invalid_argument(format!("tree: {e}")))?;
        self.meta
            .put_trees(&req.repo, std::slice::from_ref(&tree))
            .await
            .map_err(store_status)?;
        let id = tree.id();
        Ok(Response::new(PutTreeResponse {
            id: id.as_bytes().to_vec(),
        }))
    }

    type GetObjectsStream = Objects;

    async fn get_objects(
        &self,
        request: Request<GetObjectsRequest>,
    ) -> Result<Response<Self::GetObjectsStream>, Status> {
        let _rpc = span("GetObjects");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
        let req = request.into_inner();
        let n = req.trees.len() + req.subtrees.len() + req.blobs.len();
        if n > MAX_IDS {
            return Err(too_many_ids(n));
        }
        let ids = |raw: &[Vec<u8>]| {
            raw.iter()
                .map(|b| tree_id(b))
                .collect::<Result<Vec<_>, _>>()
        };
        let plain = ids(&req.trees)?;
        let roots = ids(&req.subtrees)?;
        let blobs = req
            .blobs
            .iter()
            .map(|b| blob_id(b))
            .collect::<Result<Vec<_>, _>>()?;

        let plain: Vec<_> = trees(&*self.meta, &req.repo, &plain)
            .await?
            .iter()
            .map(|(id, t)| Ok(tree_object(*id, t)))
            .collect();
        let objects = self.objects.clone();
        let blobs = stream::iter(blobs)
            .map(move |id| {
                let objects = objects.clone();
                async move {
                    let data = objects
                        .get_blob(id)
                        .await
                        .map_err(store_status)?
                        .ok_or_else(|| Status::not_found(format!("blob {id}")))?;
                    Ok(Object {
                        id: id.as_bytes().to_vec(),
                        object: Some(object::Object::Blob(data.to_vec())),
                    })
                }
            })
            .buffer_unordered(OBJECT_CONCURRENCY);
        let out = stream::iter(plain)
            .chain(subtrees(self.meta.clone(), req.repo, roots))
            .chain(blobs);
        Ok(Response::new(Box::pin(out)))
    }

    async fn has_blobs(
        &self,
        request: Request<HasBlobsRequest>,
    ) -> Result<Response<HasBlobsResponse>, Status> {
        let _rpc = span("HasBlobs");
        timed(
            "auth",
            authorize(&*self.meta, &request, &request.get_ref().repo, Role::Reader),
        )
        .await?;
        let req = request.get_ref();
        if req.ids.len() > MAX_IDS {
            return Err(too_many_ids(req.ids.len()));
        }
        let ids = req
            .ids
            .iter()
            .map(|b| blob_id(b))
            .collect::<Result<Vec<_>, _>>()?;
        let present = self.objects.has_blobs(&ids).await.map_err(store_status)?;
        Ok(Response::new(HasBlobsResponse { present }))
    }

    async fn put_blobs(
        &self,
        mut request: Request<Streaming<PutBlobsRequest>>,
    ) -> Result<Response<PutBlobsResponse>, Status> {
        let _rpc = span("PutBlobs");
        let Some(first) = request.get_mut().message().await? else {
            return Ok(Response::new(PutBlobsResponse {}));
        };
        let repo = first.repo.clone();
        timed(
            "auth",
            authorize(&*self.meta, &request, &repo, Role::Reader),
        )
        .await?;
        let rest = request.into_inner();
        stream::once(async { Ok(first) })
            .chain(rest)
            .map(|msg| {
                let objects = self.objects.clone();
                let repo = &repo;
                async move {
                    let msg = msg?;
                    if msg.repo != *repo {
                        return Err(Status::invalid_argument(format!(
                            "every message must name repository {repo:?}, not {:?}",
                            msg.repo
                        )));
                    }
                    if msg.data.len() > MAX_BLOB_BYTES {
                        return Err(Status::resource_exhausted(blob_too_large(msg.data.len())));
                    }
                    objects
                        .put_blob(Bytes::from(msg.data))
                        .await
                        .map_err(store_status)?;
                    Ok(())
                }
            })
            .buffer_unordered(OBJECT_CONCURRENCY)
            .try_collect::<()>()
            .await?;
        Ok(Response::new(PutBlobsResponse {}))
    }
}
