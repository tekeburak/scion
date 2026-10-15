//! A generated 20,000-commit history: import time per commit must not grow
//! with the history, and the importer's memory must stay bounded.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::git;
use scion_ingest::{GitRepo, import};
use scion_store::MemoryStore;

const REPO: &str = "big";
const COMMITS: usize = 20_000;
/// Commits timed at the start and at the end of the history.
const WINDOW: usize = 2_000;

/// A linear history where commit `i` rewrites one of 500 files in 20
/// directories, written with `git fast-import`.
fn generate(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    let mut stream = Vec::new();
    for i in 1..=COMMITS {
        let msg = format!("commit {i}\n");
        let body = format!("line one\nversion {i}\nline three\n");
        writeln!(stream, "commit refs/heads/main").unwrap();
        writeln!(stream, "mark :{i}").unwrap();
        writeln!(
            stream,
            "committer Gen <gen@example.com> {} +0000",
            1_700_000_000 + i
        )
        .unwrap();
        writeln!(stream, "data {}\n{msg}", msg.len()).unwrap();
        if i > 1 {
            writeln!(stream, "from :{}", i - 1).unwrap();
        }
        writeln!(stream, "M 100644 inline dir{}/file{}.txt", i % 20, i % 500).unwrap();
        writeln!(stream, "data {}\n{body}", body.len()).unwrap();
    }
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("git is installed");
    child.stdin.take().unwrap().write_all(&stream).unwrap();
    assert!(child.wait().unwrap().success());
}

/// Peak resident memory of this process in KiB, where the OS reports it.
fn peak_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

async fn timed(git: &GitRepo, rev: &str, store: &MemoryStore) -> Duration {
    let start = Instant::now();
    import(git, rev, REPO, store, store).await.unwrap();
    start.elapsed()
}

#[tokio::test]
async fn per_commit_cost_does_not_grow_with_history() {
    let tmp = tempfile::tempdir().unwrap();
    generate(tmp.path());
    let git = GitRepo::open(tmp.path()).unwrap();
    let store = MemoryStore::new();

    let first = timed(&git, &format!("main~{}", COMMITS - WINDOW), &store).await;
    timed(&git, &format!("main~{WINDOW}"), &store).await;
    let last = timed(&git, "main", &store).await;
    eprintln!("first {WINDOW} commits: {first:?}, last {WINDOW}: {last:?}");
    assert!(
        last < first * 3,
        "the last {WINDOW} commits took {last:?}, the first {WINDOW} {first:?}"
    );

    if let Some(kib) = peak_rss_kib() {
        eprintln!("peak RSS: {} MiB", kib / 1024);
        // The memory store itself holds every blob, tree and derived value;
        // the bound is generous for that, and far below what keeping
        // per-commit file contents around would cost.
        assert!(kib < 1024 * 1024, "peak RSS {} MiB", kib / 1024);
    }
}
