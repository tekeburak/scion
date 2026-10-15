//! [`ObjectStore`] over the `object_store` crate.
//!
//! One implementation serves S3/MinIO, GCS, Azure, the local filesystem and an
//! in-memory backend. Keys are `blobs/{id}` and `trees/{id}`; values are
//! zstd-compressed. Every read is verified against its id, so a corrupt or
//! tampered object is reported rather than returned.

use std::cell::RefCell;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{ObjectStore as Backend, ObjectStoreExt as _, PutMode, PutOptions, PutPayload};
use scion_types::{BlobId, Decode, Encode, Tree, TreeId};

use crate::{ObjectStore, Result, StoreError};

/// Default zstd compression level.
pub const ZSTD_LEVEL: i32 = 3;

/// How many `HEAD` requests `has_blobs` issues concurrently.
const HAS_CONCURRENCY: usize = 32;

/// An [`ObjectStore`] backed by any `object_store` backend.
#[derive(Debug, Clone)]
pub struct ObjectStorage {
    backend: Arc<dyn Backend>,
}

impl From<object_store::Error> for StoreError {
    fn from(e: object_store::Error) -> Self {
        StoreError::Backend(Box::new(e))
    }
}

fn blob_key(id: BlobId) -> Path {
    Path::from(format!("blobs/{id}"))
}

fn tree_key(id: TreeId) -> Path {
    Path::from(format!("trees/{id}"))
}

fn compress(data: &[u8]) -> Result<Vec<u8>> {
    // Setting up a compression context costs more than compressing a small
    // tree, so each thread keeps one.
    thread_local! {
        static COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> =
            const { RefCell::new(None) };
    }
    COMPRESSOR
        .with_borrow_mut(|c| {
            let c = match c {
                Some(c) => c,
                None => c.insert(zstd::bulk::Compressor::new(ZSTD_LEVEL)?),
            };
            c.compress(data)
        })
        .map_err(|e| StoreError::Backend(Box::new(e)))
}

fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    zstd::decode_all(data).map_err(|e| StoreError::Corrupt(format!("zstd: {e}")))
}

impl ObjectStorage {
    /// Wrap a backend.
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    /// An in-memory backend, for tests.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::new(Arc::new(object_store::memory::InMemory::new()))
    }

    /// A local-filesystem backend rooted at `root`.
    pub fn local(root: impl AsRef<std::path::Path>) -> Result<Self> {
        let fs = object_store::local::LocalFileSystem::new_with_prefix(root)?;
        Ok(Self::new(Arc::new(fs)))
    }

    /// The underlying backend.
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    async fn exists(&self, key: &Path) -> Result<bool> {
        match self.backend.head(key).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Store `data` under `key` unless it is already present, in one
    /// conditional request (`If-None-Match: *` on S3): an existing object is
    /// never overwritten, and a write costs one round trip, not a check and
    /// a write.
    async fn put_if_absent(&self, key: Path, data: &[u8]) -> Result<()> {
        let payload = PutPayload::from(compress(data)?);
        let create = PutOptions::from(PutMode::Create);
        match self.backend.put_opts(&key, payload, create).await {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn get_raw(&self, key: &Path) -> Result<Option<Vec<u8>>> {
        match self.backend.get(key).await {
            Ok(res) => Ok(Some(decompress(&res.bytes().await?)?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl ObjectStore for ObjectStorage {
    async fn put_blob(&self, bytes: Bytes) -> Result<BlobId> {
        let id = BlobId::hash(&bytes);
        self.put_if_absent(blob_key(id), &bytes).await?;
        Ok(id)
    }

    async fn get_blob(&self, id: BlobId) -> Result<Option<Bytes>> {
        let Some(data) = self.get_raw(&blob_key(id)).await? else {
            return Ok(None);
        };
        if BlobId::hash(&data) != id {
            return Err(StoreError::Corrupt(format!(
                "blob {id} does not match its content"
            )));
        }
        Ok(Some(Bytes::from(data)))
    }

    async fn has_blobs(&self, ids: &[BlobId]) -> Result<Vec<bool>> {
        stream::iter(ids.iter().copied())
            .map(|id| async move { self.exists(&blob_key(id)).await })
            .buffered(HAS_CONCURRENCY)
            .try_collect()
            .await
    }

    async fn put_tree(&self, tree: &Tree) -> Result<TreeId> {
        let id = tree.id();
        self.put_if_absent(tree_key(id), &tree.encode_to_vec())
            .await?;
        Ok(id)
    }

    async fn get_tree(&self, id: TreeId) -> Result<Option<Tree>> {
        let Some(data) = self.get_raw(&tree_key(id)).await? else {
            return Ok(None);
        };
        let tree =
            Tree::decode(&data).map_err(|e| StoreError::Corrupt(format!("tree {id}: {e}")))?;
        if tree.id() != id {
            return Err(StoreError::Corrupt(format!(
                "tree {id} hashes to {}",
                tree.id()
            )));
        }
        Ok(Some(tree))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::object_conformance_tests!(memory_backend, async { ObjectStorage::in_memory() });

    crate::object_conformance_tests!(local_fs_backend, async {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the directory outlives the store for the test's duration.
        let path = dir.keep();
        ObjectStorage::local(path).unwrap()
    });

    #[tokio::test]
    async fn corrupt_blob_is_detected() {
        let store = ObjectStorage::in_memory();
        let id = store.put_blob(Bytes::from_static(b"good")).await.unwrap();
        // Overwrite with different (validly compressed) content.
        store
            .backend
            .put(&blob_key(id), PutPayload::from(compress(b"evil").unwrap()))
            .await
            .unwrap();
        assert!(matches!(
            store.get_blob(id).await,
            Err(StoreError::Corrupt(_))
        ));
    }

    #[tokio::test]
    async fn undecodable_tree_is_detected() {
        let store = ObjectStorage::in_memory();
        let id = Tree::empty().id();
        store
            .backend
            .put(
                &tree_key(id),
                PutPayload::from(compress(b"not a tree").unwrap()),
            )
            .await
            .unwrap();
        assert!(matches!(
            store.get_tree(id).await,
            Err(StoreError::Corrupt(_))
        ));
    }
}
