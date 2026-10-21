//! `scion-ingest` binary.
//!
//! ```text
//! scion-ingest import <git-dir> <repo> [<rev>]
//! scion-ingest verify <git-dir> <repo> [--sample <n>]
//! scion-ingest --version
//! ```
//!
//! `import` imports every commit reachable from `<rev>` (default `HEAD`) and
//! points `main` at it; an interrupted import resumes where it stopped.
//! `verify` compares every imported commit (or `n` of them, spread evenly)
//! with the git tree and names the first path that differs. Like `scion-server`, it works on the stores directly and is
//! configured by the environment: `DATABASE_URL` for Postgres and the `AWS_*`
//! variables read by `object_store`'s `AmazonS3Builder::from_env`.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use object_store::aws::AmazonS3Builder;
use scion_ingest::{GitRepo, MAIN_REF, import, verify};
use scion_store::{ObjectStorage, PostgresStore};

const USAGE: &str = "usage: scion-ingest import <git-dir> <repo> [<rev>]\n       \
                     scion-ingest verify <git-dir> <repo> [--sample <n>]";

enum Cmd<'a> {
    Import { rev: &'a str },
    Verify { sample: Option<usize> },
}

async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let (cmd, git_dir, repo) = match args {
        [v] if v == "--version" => {
            println!("scion-ingest {}", scion_types::version());
            return Ok(());
        }
        [c, git_dir, repo] if c == "import" => (Cmd::Import { rev: "HEAD" }, git_dir, repo),
        [c, git_dir, repo, rev] if c == "import" => (Cmd::Import { rev }, git_dir, repo),
        [c, git_dir, repo] if c == "verify" => (Cmd::Verify { sample: None }, git_dir, repo),
        [c, git_dir, repo, flag, n] if c == "verify" && flag == "--sample" => {
            let n = n.parse().map_err(|_| USAGE)?;
            (Cmd::Verify { sample: Some(n) }, git_dir, repo)
        }
        _ => return Err(USAGE.into()),
    };
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let meta = PostgresStore::connect(&database_url).await?;
    meta.migrate().await?;
    let objects = ObjectStorage::new(Arc::new(AmazonS3Builder::from_env().build()?));

    let git = GitRepo::open(Path::new(git_dir))?;
    match cmd {
        Cmd::Import { rev } => {
            let done = import(&git, rev, repo, &meta, &objects).await?;
            println!(
                "imported {rev} into {repo}: {} new changesets, {MAIN_REF} is at {}",
                done.created, done.tip
            );
        }
        Cmd::Verify { sample } => {
            let checked = verify(&git, repo, &meta, &objects, sample).await?;
            println!("verified {checked} commits of {repo} against git");
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
            eprintln!("scion-ingest: {e}");
            ExitCode::FAILURE
        }
    }
}
