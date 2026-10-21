//! Conformance suite against a real Postgres.
//!
//! Requires `--features integration` and a reachable server (default: the one
//! from `deploy/docker-compose.yml`). Override with `SCION_TEST_DATABASE_URL`.
//! Each test gets its own freshly migrated database; stale databases from
//! earlier runs are dropped on the next run.

#![cfg(feature = "integration")]

use std::sync::atomic::{AtomicU64, Ordering};

use scion_store::PostgresStore;
use sqlx::postgres::{PgConnectOptions, PgPool};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn base_options() -> PgConnectOptions {
    std::env::var("SCION_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://scion:scion@localhost:5432/scion".to_owned())
        .parse()
        .expect("SCION_TEST_DATABASE_URL is a valid postgres URL")
}

async fn drop_stale_databases(admin: &PgPool, prefix: &str) {
    let names: Vec<(String,)> =
        sqlx::query_as("SELECT datname FROM pg_database WHERE datname LIKE 'scion_test_%'")
            .fetch_all(admin)
            .await
            .unwrap();
    for (name,) in names {
        if !name.starts_with(prefix) {
            // Ignore failures: another test process may own it.
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                .execute(admin)
                .await;
        }
    }
}

async fn fresh() -> PostgresStore {
    let opts = base_options();
    let prefix = format!("scion_test_{}_", std::process::id());
    let name = format!("{prefix}{}", COUNTER.fetch_add(1, Ordering::Relaxed));

    let admin = PgPool::connect_with(opts.clone())
        .await
        .expect("connect to postgres");
    drop_stale_databases(&admin, &prefix).await;
    sqlx::query(&format!("CREATE DATABASE \"{name}\""))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;

    let store = PostgresStore::connect_with(opts.database(&name))
        .await
        .unwrap();
    store.migrate().await.unwrap();
    store
}

scion_store::metadata_conformance_tests!(postgres, fresh());

/// Trees and pages read back through a fresh memo come from the tables,
/// checked against their ids.
#[tokio::test]
async fn trees_and_pages_are_read_from_the_database() {
    use std::collections::BTreeMap;

    use bytes::Bytes;
    use scion_store::{MetadataStore, StoreError};
    use scion_types::{BlobId, FileType, PathComponent, Tree, TreeEntry};

    let writer = fresh().await;
    writer.create_repo("r").await.unwrap();
    let tree = Tree {
        entries: BTreeMap::from([(
            PathComponent::new("f").unwrap(),
            TreeEntry::Blob {
                id: BlobId::hash(b"f"),
                kind: FileType::Regular,
                size: 1,
            },
        )]),
    };
    let page = Bytes::from_static(b"history nodes");
    writer
        .put_trees("r", std::slice::from_ref(&tree))
        .await
        .unwrap();
    writer
        .put_pages("r", std::slice::from_ref(&page))
        .await
        .unwrap();

    let reader = PostgresStore::from_pool(writer.pool().clone());
    let empty = Tree::empty().id();
    assert_eq!(
        reader.get_trees("r", &[empty, tree.id()]).await.unwrap(),
        [None, Some(tree.clone())]
    );
    let pid = BlobId::hash(&page);
    assert_eq!(reader.get_pages("r", &[pid]).await.unwrap(), [Some(page)]);

    sqlx::query("UPDATE pages SET bytes = 'tampered'")
        .execute(writer.pool())
        .await
        .unwrap();
    let fresh_reader = PostgresStore::from_pool(writer.pool().clone());
    assert!(matches!(
        fresh_reader.get_pages("r", &[pid]).await,
        Err(StoreError::Corrupt(_))
    ));
}
