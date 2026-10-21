//! An import that stops part-way resumes from its checkpoints.

mod common;

use async_trait::async_trait;
use bytes::Bytes;
use common::fixture;
use scion_ingest::{GitRepo, MAIN_REF, import};
use scion_store::{MemoryStore, MetadataStore, ObjectStore, StoreError};
use scion_types::{BlobId, Changeset};

const REPO: &str = "mono";

/// An object store that fails the upload of one file's contents, as if the
/// importer were killed while importing the commit that adds it.
struct DiesOn<'a> {
    inner: &'a MemoryStore,
    contents: &'static [u8],
}

#[async_trait]
impl ObjectStore for DiesOn<'_> {
    async fn put_blob(&self, bytes: Bytes) -> scion_store::Result<BlobId> {
        if bytes == self.contents {
            return Err(StoreError::Backend("killed".into()));
        }
        self.inner.put_blob(bytes).await
    }
    async fn get_blob(&self, id: BlobId) -> scion_store::Result<Option<Bytes>> {
        self.inner.get_blob(id).await
    }
    async fn has_blobs(&self, ids: &[BlobId]) -> scion_store::Result<Vec<bool>> {
        self.inner.has_blobs(ids).await
    }
}

#[tokio::test]
async fn stopped_import_resumes_with_the_remaining_commits() {
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path());
    let git = GitRepo::open(tmp.path()).unwrap();

    let reference = MemoryStore::new();
    common::init(&reference, &reference, REPO).await;
    let uninterrupted = import(&git, "main", REPO, &reference, &reference)
        .await
        .unwrap();

    // Parents first, the third commit is "file to directory": die there.
    let store = MemoryStore::new();
    common::init(&store, &store, REPO).await;
    let dying = DiesOn {
        inner: &store,
        contents: b"// a file became a directory\n",
    };
    assert!(import(&git, "main", REPO, &store, &dying).await.is_err());
    let done = store.ingest_checkpoints(REPO).await.unwrap().len();
    assert_eq!(done, 2);
    assert_eq!(
        store.get_ref(REPO, MAIN_REF).await.unwrap().unwrap().target,
        Changeset::repository_root(REPO).id(),
        "main is still at the empty root"
    );

    let resumed = import(&git, "main", REPO, &store, &store).await.unwrap();
    assert_eq!(resumed.tip, uninterrupted.tip);
    assert_eq!(resumed.created, uninterrupted.created - done);
    assert_eq!(
        store.get_ref(REPO, MAIN_REF).await.unwrap().unwrap().target,
        uninterrupted.tip
    );
}
