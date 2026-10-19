//! `scion-server` binary.
//!
//! ```text
//! scion-server                                      serve
//! scion-server backfill <repo> <type>               derive <type> for the whole history
//! scion-server grant <repo|*> <principal> <role> [--path <dir>]
//! scion-server revoke <repo|*> <principal> [--path <dir>]
//! scion-server token <principal> --ttl <duration>   print a token
//! ```
//!
//! `backfill` derives one derived type (`root_tree`, `path_history`,
//! `skip_list`) for every changeset reachable from the repository's refs that
//! lacks it, so the first request after a `VERSION` bump does not pay for the
//! whole history. It can run while the server serves, and resumes where an
//! interrupted run stopped.
//!
//! `grant` gives a principal (`github:<login>` or `sp:<name>`) a role
//! (`reader`, `writer`, `admin`) in a repository, under `--path` if given;
//! `grant '*' <principal> admin` lets it create repositories. `revoke` removes
//! a grant. `token` prints a token for a principal, valid for `--ttl` (`90s`,
//! `30m`, `12h`, `30d`): a service principal's, or the first admin's before
//! anyone can log in. All three work on the stores directly.
//!
//! Configuration is the environment:
//!
//! * `DATABASE_URL`: Postgres metadata store (migrated on start).
//! * `AWS_*`: the S3-compatible object store, as read by `object_store`'s
//!   `AmazonS3Builder::from_env` (`AWS_ENDPOINT`, `AWS_BUCKET`,
//!   `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`,
//!   `AWS_ALLOW_HTTP`, ...).
//! * `SCION_LISTEN`: listen address, default `0.0.0.0:7420`.
//! * `SCION_TOKEN_KEY`: the 32-byte key tokens are signed with, in hex
//!   (`openssl rand -hex 32`). `serve` and `token` refuse to run without it.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;

use object_store::aws::AmazonS3Builder;
use scion_server::{Command, TokenKey};
use scion_store::{ObjectStorage, PostgresStore};
use tokio::net::TcpListener;

/// The Postgres metadata store, migrated.
async fn metadata() -> Result<PostgresStore, Box<dyn std::error::Error + Send + Sync>> {
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let meta = PostgresStore::connect(&database_url).await?;
    meta.migrate().await?;
    Ok(meta)
}

/// The token key from `SCION_TOKEN_KEY`.
fn token_key() -> Result<TokenKey, Box<dyn std::error::Error + Send + Sync>> {
    let key = std::env::var("SCION_TOKEN_KEY")
        .map_err(|_| "SCION_TOKEN_KEY is not set (make one with `openssl rand -hex 32`)")?;
    Ok(TokenKey::from_hex(&key)?)
}

async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match Command::parse(args)? {
        Command::Serve => {
            let key = token_key()?;
            let listen =
                std::env::var("SCION_LISTEN").unwrap_or_else(|_| "0.0.0.0:7420".to_owned());
            let meta = metadata().await?;
            let objects = ObjectStorage::new(Arc::new(AmazonS3Builder::from_env().build()?));
            let listener = TcpListener::bind(&listen).await?;
            eprintln!("scion-server listening on {}", listener.local_addr()?);
            scion_server::serve(listener, Arc::new(meta), Arc::new(objects), key).await?;
        }
        Command::Backfill { repo, ty } => {
            let meta = metadata().await?;
            let objects = ObjectStorage::new(Arc::new(AmazonS3Builder::from_env().build()?));
            let refs = scion_server::backfill(&repo, &ty, &meta, &objects).await?;
            println!("derived {ty} for every changeset reachable from the {refs} refs of {repo}");
        }
        Command::Grant { repo, grant } => {
            let meta = metadata().await?;
            println!("{}", scion_server::grant(&meta, &repo, &grant).await?);
        }
        Command::Revoke {
            repo,
            principal,
            path_prefix,
        } => {
            let meta = metadata().await?;
            let done = scion_server::revoke(&meta, &repo, &principal, &path_prefix).await?;
            println!("{done}");
        }
        Command::Token { principal, ttl } => {
            let token = scion_server::token(&token_key()?, &principal, ttl, SystemTime::now())?;
            println!("{token}");
        }
    }
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
