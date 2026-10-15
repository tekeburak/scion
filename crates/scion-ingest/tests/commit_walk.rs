//! `commits` on a fixture repository built with the `git` CLI.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use scion_ingest::{GitRepo, IngestError, ObjectId};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "a")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "a")
        .env("GIT_COMMITTER_EMAIL", "a@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git is installed");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn commit(dir: &Path, file: &str, msg: &str) {
    std::fs::write(dir.join(file), msg).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", msg]);
}

/// main: a ─ b ─────── m1 ─ e ─ m2
///            \       /         /
///  feature:   c ─ d ─         /
///                  \         /
///  side:            f ───────
fn fixture(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    commit(dir, "a", "a");
    commit(dir, "b", "b");
    git(dir, &["checkout", "-q", "-b", "feature"]);
    commit(dir, "c", "c");
    commit(dir, "d", "d");
    git(dir, &["checkout", "-q", "-b", "side"]);
    commit(dir, "f", "f");
    git(dir, &["checkout", "-q", "main"]);
    git(dir, &["merge", "-q", "--no-ff", "-m", "m1", "feature"]);
    commit(dir, "e", "e");
    git(dir, &["merge", "-q", "--no-ff", "-m", "m2", "side"]);
}

fn rev_list(dir: &Path, rev: &str) -> HashSet<ObjectId> {
    git(dir, &["rev-list", rev])
        .lines()
        .map(|l| ObjectId::from_hex(l.as_bytes()).unwrap())
        .collect()
}

#[test]
fn lists_every_reachable_commit_parents_first() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fixture(dir);

    let repo = GitRepo::open(dir).unwrap();
    let walk = repo.commits("main").unwrap();
    let ids: Vec<ObjectId> = walk.iter().map(|c| c.id).collect();
    assert_eq!(ids.len(), 8);
    assert_eq!(
        ids.iter().copied().collect::<HashSet<_>>(),
        rev_list(dir, "main")
    );

    let position: HashMap<ObjectId, usize> =
        ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    for c in &walk {
        for p in &c.parents {
            assert!(
                position[p] < position[&c.id],
                "parent {p} after child {}",
                c.id
            );
        }
        let expected: Vec<ObjectId> =
            git(dir, &["rev-list", "--parents", "-n1", &c.id.to_string()])
                .split_whitespace()
                .skip(1)
                .map(|h| ObjectId::from_hex(h.as_bytes()).unwrap())
                .collect();
        assert_eq!(
            c.parents, expected,
            "parents of {}, first parent first",
            c.id
        );
    }
    assert_eq!(
        ids.last(),
        Some(&ObjectId::from_hex(git(dir, &["rev-parse", "main"]).trim().as_bytes()).unwrap())
    );

    // Deterministic, and any revision works.
    assert_eq!(repo.commits("main").unwrap(), walk);
    assert_eq!(repo.commits("feature").unwrap().len(), 4);
    assert_eq!(repo.commits("main~1").unwrap().len(), 6);
}

#[test]
fn reports_bad_inputs() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(matches!(
        GitRepo::open(tmp.path()),
        Err(IngestError::Open(..))
    ));
    fixture(tmp.path());
    let repo = GitRepo::open(tmp.path()).unwrap();
    assert!(matches!(
        repo.commits("no-such-ref"),
        Err(IngestError::Rev(..))
    ));
}
