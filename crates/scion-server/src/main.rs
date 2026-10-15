//! `scion-server` binary.
//!
//! Configuration is the environment:
//!
//! * `DATABASE_URL`: Postgres metadata store (migrated on start).
//! * `AWS_*`: the S3-compatible object store, as read by `object_store`'s
//!   `AmazonS3Builder::from_env` (`AWS_ENDPOINT`, `AWS_BUCKET`,
//!   `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`,
//!   `AWS_ALLOW_HTTP`, ...).
//! * `SCION_LISTEN`: listen address, default `0.0.0.0:7420`.

use std::sync::Arc;

use object_store::aws::AmazonS3Builder;
use scion_store::{ObjectStorage, PostgresStore};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let listen = std::env::var("SCION_LISTEN").unwrap_or_else(|_| "0.0.0.0:7420".to_owned());

    let meta = PostgresStore::connect(&database_url).await?;
    meta.migrate().await?;
    let objects = ObjectStorage::new(Arc::new(AmazonS3Builder::from_env().build()?));

    let listener = TcpListener::bind(&listen).await?;
    eprintln!("scion-server listening on {}", listener.local_addr()?);
    scion_server::serve(listener, Arc::new(meta), Arc::new(objects)).await?;
    Ok(())
}
