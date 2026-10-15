//! `scion-ingest` binary.
//!
//! ```text
//! scion-ingest import <git-dir> <repo> [<rev>]
//! ```
//!
//! Imports every commit reachable from `<rev>` (default `HEAD`) and points
//! `main` at it. An interrupted import resumes where it stopped. Like `scion-server`, it works on the stores directly and is
//! configured by the environment: `DATABASE_URL` for Postgres and the `AWS_*`
//! variables read by `object_store`'s `AmazonS3Builder::from_env`.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use object_store::aws::AmazonS3Builder;
use scion_ingest::{GitRepo, MAIN_REF, import};
use scion_store::{ObjectStorage, PostgresStore};

const USAGE: &str = "usage: scion-ingest import <git-dir> <repo> [<rev>]";

async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let (git_dir, repo, rev) = match args {
        [cmd, git_dir, repo] if cmd == "import" => (git_dir, repo, "HEAD"),
        [cmd, git_dir, repo, rev] if cmd == "import" => (git_dir, repo, rev.as_str()),
        _ => return Err(USAGE.into()),
    };
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let meta = PostgresStore::connect(&database_url).await?;
    meta.migrate().await?;
    let objects = ObjectStorage::new(Arc::new(AmazonS3Builder::from_env().build()?));

    let git = GitRepo::open(Path::new(git_dir))?;
    let done = import(&git, rev, repo, &meta, &objects).await?;
    println!(
        "imported {rev} into {repo}: {} new changesets, {MAIN_REF} is at {}",
        done.created, done.tip
    );
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("scion-ingest: {e}");
            ExitCode::FAILURE
        }
    }
}
