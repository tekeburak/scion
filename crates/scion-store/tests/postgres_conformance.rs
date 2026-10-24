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

/// A [`fresh`] store that memoises refs and grants.
async fn listening() -> PostgresStore {
    let store = fresh().await;
    store.listen().await.unwrap();
    store
}

scion_store::metadata_conformance_tests!(postgres, fresh());
scion_store::metadata_conformance_tests!(postgres_listening, listening());

/// Wait up to five seconds for `check` to hold.
async fn eventually<F: Future<Output = bool>>(mut check: impl FnMut() -> F) {
    for _ in 0..100 {
        if check().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("not observed within five seconds");
}

/// While listening, a repeated read of refs or grants makes no query: rows
/// changed behind the store's back, without a notification, are not seen.
/// A write through another store handle (another process, such as
/// `scion-server revoke` or `scion-ingest import`) is seen once its
/// notification arrives, a write through this one at once, and losing the
/// listener's connection drops the memo.
#[tokio::test]
async fn refs_and_grants_memos_follow_notified_writes() {
    use scion_store::{Grant, MetadataStore, RefState, Role};
    use scion_types::ChangesetId;

    let server = listening().await;
    let operator = PostgresStore::from_pool(server.pool().clone());
    let pool = server.pool().clone();
    server.create_repo("r").await.unwrap();
    let grant = |path_prefix: &str| Grant {
        principal: "sp:a".into(),
        path_prefix: path_prefix.into(),
        role: Role::Reader,
    };
    let id = |n: u8| ChangesetId::from_bytes([n; 32]);
    let at = |n: u8, version: u64| RefState {
        target: id(n),
        version,
    };
    // A write that sends no notification.
    let silently = |sql: String| {
        let pool = pool.clone();
        async move { sqlx::query(&sql).execute(&pool).await.unwrap() }
    };
    let add_grant = |prefix: &str| {
        format!(
            "INSERT INTO grants (repo, principal, path_prefix, role) \
             SELECT id, 'sp:a', '{prefix}', 'reader' FROM repos"
        )
    };
    let set_main = |n: u8, version: u64| {
        format!(
            "INSERT INTO refs (repo, name, target, version) \
             SELECT id, 'main', '\\x{}', {version} FROM repos \
             ON CONFLICT (repo, name) DO UPDATE SET target = EXCLUDED.target, \
             version = EXCLUDED.version",
            id(n)
        )
    };

    silently(add_grant("")).await;
    silently(set_main(1, 0)).await;
    assert_eq!(server.grants_for("r", "sp:a").await.unwrap(), [grant("")]);
    assert_eq!(server.get_ref("r", "main").await.unwrap(), Some(at(1, 0)));

    silently(add_grant("x")).await;
    silently(set_main(2, 1)).await;
    assert_eq!(server.grants_for("r", "sp:a").await.unwrap(), [grant("")]);
    assert_eq!(server.get_ref("r", "main").await.unwrap(), Some(at(1, 0)));

    assert!(operator.delete_grant("r", "sp:a", "").await.unwrap());
    operator.move_ref("r", "main", 1, id(3)).await.unwrap();
    eventually(|| async { server.grants_for("r", "sp:a").await.unwrap() == [grant("x")] }).await;
    eventually(|| async { server.get_ref("r", "main").await.unwrap() == Some(at(3, 2)) }).await;

    // A ref write, and the notification it sends, leave the grants memo be.
    silently(add_grant("y")).await;
    server.move_ref("r", "main", 2, id(4)).await.unwrap();
    assert_eq!(server.get_ref("r", "main").await.unwrap(), Some(at(4, 3)));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(server.grants_for("r", "sp:a").await.unwrap(), [grant("x")]);

    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = current_database() AND query LIKE 'LISTEN%'",
    )
    .execute(&pool)
    .await
    .unwrap();
    silently(set_main(1, 9)).await;
    eventually(|| async { server.get_ref("r", "main").await.unwrap() == Some(at(1, 9)) }).await;
}

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
