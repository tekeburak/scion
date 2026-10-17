//! `scion-server` binary.
//!
//! ```text
//! scion-server                        serve
//! scion-server backfill <repo> <type> derive <type> for the whole history
//! ```
//!
//! `backfill` derives one derived type (`root_tree`, `path_history`,
//! `skip_list`) for every changeset reachable from the repository's refs that
//! lacks it, so the first request after a `VERSION` bump does not pay for the
//! whole history. It can run while the server serves, and resumes where an
//! interrupted run stopped.
//!
//! Configuration is the environment:
//!
//! * `DATABASE_URL`: Postgres metadata store (migrated on start).
//! * `AWS_*`: the S3-compatible object store, as read by `object_store`'s
//!   `AmazonS3Builder::from_env` (`AWS_ENDPOINT`, `AWS_BUCKET`,
//!   `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`,
//!   `AWS_ALLOW_HTTP`, ...).
//! * `SCION_LISTEN`: listen address, default `0.0.0.0:7420`.

use std::process::ExitCode;
use std::sync::Arc;

use object_store::aws::AmazonS3Builder;
use scion_store::{ObjectStorage, PostgresStore};
use tokio::net::TcpListener;

const USAGE: &str = "usage: scion-server\n       scion-server backfill <repo> <type>";

async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let backfill = match args {
        [] => None,
        [c, repo, ty] if c == "backfill" => Some((repo, ty)),
        _ => return Err(USAGE.into()),
    };
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let listen = std::env::var("SCION_LISTEN").unwrap_or_else(|_| "0.0.0.0:7420".to_owned());

    let meta = PostgresStore::connect(&database_url).await?;
    meta.migrate().await?;
    let objects = ObjectStorage::new(Arc::new(AmazonS3Builder::from_env().build()?));
    if let Some((repo, ty)) = backfill {
        let refs = scion_server::backfill(repo, ty, &meta, &objects).await?;
        println!("derived {ty} for every changeset reachable from the {refs} refs of {repo}");
        return Ok(());
    }

    let listener = TcpListener::bind(&listen).await?;
    eprintln!("scion-server listening on {}", listener.local_addr()?);
    scion_server::serve(listener, Arc::new(meta), Arc::new(objects)).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("scion-server: {e}");
            ExitCode::FAILURE
        }
    }
}
