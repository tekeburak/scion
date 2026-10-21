//! [`ObjectStore`] over the `object_store` crate.
//!
//! One implementation serves S3/MinIO, GCS, Azure, the local filesystem and an
//! in-memory backend. It holds blobs only, under `blobs/{id}`, zstd-compressed;
//! trees and path-history pages live in the metadata store. Every read is
//! verified against its id, so a corrupt or tampered object is reported
//! rather than returned.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{ObjectStore as Backend, ObjectStoreExt as _, PutMode, PutPayload};
use scion_types::BlobId;

use crate::recent::Recent;
use crate::{ObjectStore, Result, StoreError};

/// Default zstd compression level.
pub const ZSTD_LEVEL: i32 = 3;

/// How many `HEAD` requests `has_blobs` issues concurrently.
const HAS_CONCURRENCY: usize = 32;

/// Bytes of recently read or written small blobs kept in memory.
const BLOB_MEMO_BYTES: usize = 64 << 20;
/// Blobs at most this large are memoised: derived nodes and source files,
/// not large assets.
const SMALL_BLOB_BYTES: usize = 64 << 10;
/// Stored blobs at most this large are decompressed and checked on the
/// reading task; larger ones on the blocking pool, so a request that reads
/// many source files at once uses every core.
const INLINE_DECODE_BYTES: usize = 16 << 10;
/// Bytes of blob ids known to be stored.
const PRESENT_MEMO_BYTES: usize = 8 << 20;

/// An [`ObjectStore`] backed by any `object_store` backend.
///
/// Blobs are immutable, so recently read or written small blobs, and the ids
/// of blobs known to be stored, are memoised in a bounded amount of memory:
/// `CreateChangeset` checks blobs its caller just uploaded. Clones share the
/// memo.
#[derive(Debug, Clone)]
pub struct ObjectStorage {
    backend: Arc<dyn Backend>,
    blobs: Arc<Recent<BlobId, Bytes>>,
    present: Arc<Recent<BlobId, ()>>,
}

impl From<object_store::Error> for StoreError {
    fn from(e: object_store::Error) -> Self {
        StoreError::Backend(Box::new(e))
    }
}

fn blob_key(id: BlobId) -> Path {
    Path::from(format!("blobs/{id}"))
}

fn compress(data: &[u8]) -> Result<Vec<u8>> {
    zstd::encode_all(data, ZSTD_LEVEL).map_err(|e| StoreError::Backend(Box::new(e)))
}

fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    zstd::decode_all(data).map_err(|e| StoreError::Corrupt(format!("zstd: {e}")))
}

/// The blob `id` from its stored (compressed) bytes, checked against its id.
fn decode_blob(id: BlobId, compressed: &[u8]) -> Result<Bytes> {
    let data = decompress(compressed)?;
    if BlobId::hash(&data) != id {
        return Err(StoreError::Corrupt(format!(
            "blob {id} does not match its content"
        )));
    }
    Ok(Bytes::from(data))
}

impl ObjectStorage {
    /// Wrap a backend.
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            blobs: Arc::new(Recent::new(BLOB_MEMO_BYTES)),
            present: Arc::new(Recent::new(PRESENT_MEMO_BYTES)),
        }
    }

    fn remember_blob(&self, id: BlobId, bytes: &Bytes) {
        self.present.insert(id, (), BlobId::LEN);
        if bytes.len() <= SMALL_BLOB_BYTES {
            self.blobs.insert(id, bytes.clone(), bytes.len());
        }
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
    /// conditional request: an object is never overwritten, and an existing
    /// one costs no extra round trip to find out.
    async fn put_if_absent(&self, key: Path, data: &[u8]) -> Result<()> {
        let payload = PutPayload::from(compress(data)?);
        match self
            .backend
            .put_opts(&key, payload, PutMode::Create.into())
            .await
        {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl ObjectStore for ObjectStorage {
    async fn put_blob(&self, bytes: Bytes) -> Result<BlobId> {
        let id = BlobId::hash(&bytes);
        if self.present.get(&id).await.is_none() {
            self.put_if_absent(blob_key(id), &bytes).await?;
        }
        self.remember_blob(id, &bytes);
        Ok(id)
    }

    async fn get_blob(&self, id: BlobId) -> Result<Option<Bytes>> {
        if let Some(bytes) = self.blobs.get(&id).await {
            return Ok(Some(bytes));
        }
        let compressed = match self.backend.get(&blob_key(id)).await {
            Ok(res) => res.bytes().await?,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let bytes = if compressed.len() <= INLINE_DECODE_BYTES {
            decode_blob(id, &compressed)?
        } else {
            tokio::task::spawn_blocking(move || decode_blob(id, &compressed))
                .await
                .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))?
        };
        self.remember_blob(id, &bytes);
        Ok(Some(bytes))
    }

    async fn has_blobs(&self, ids: &[BlobId]) -> Result<Vec<bool>> {
        stream::iter(ids.iter().copied())
            .map(|id| async move {
                if self.present.get(&id).await.is_some() {
                    return Ok(true);
                }
                let present = self.exists(&blob_key(id)).await?;
                if present {
                    self.present.insert(id, (), BlobId::LEN);
                }
                Ok(present)
            })
            .buffered(HAS_CONCURRENCY)
            .try_collect()
            .await
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
        // A fresh store over the same backend: the memo only holds what was
        // verified when it was read or written.
        let reader = ObjectStorage::new(store.backend().clone());
        assert!(matches!(
            reader.get_blob(id).await,
            Err(StoreError::Corrupt(_))
        ));
    }
}
