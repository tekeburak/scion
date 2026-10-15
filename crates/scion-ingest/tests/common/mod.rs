//! Shared fixture: a small git history with fixed dates, built with the
//! `git` CLI, so the git shas (and with them the changeset ids) never change.

#![allow(dead_code)] // each test binary uses a different subset

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

pub fn git_with(dir: &Path, args: &[&str], date: &str, stdin: Option<&[u8]>) -> Vec<u8> {
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_NAME", "Bob")
        .env("GIT_COMMITTER_EMAIL", "bob@example.com")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05+0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git is installed");
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git_with(dir, args, "2026-01-01T00:00:00+0300", None)).unwrap()
}

pub fn write(dir: &Path, path: &str, data: &str) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

pub fn commit(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
}

/// main: root ─ edit ─ file→dir ─ merge
///                  \               /
///   side:           side work ─────
pub fn fixture(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    write(dir, "README", "one\n");
    write(dir, "src/lib.rs", "pub fn f() {}\n");
    write(dir, "bin/run", "#!/bin/sh\n");
    std::fs::set_permissions(dir.join("bin/run"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("../README", dir.join("src/link")).unwrap();
    write(dir, "gone/x", "x\n");
    commit(dir, "root");

    write(dir, "README", "one\ntwo\n");
    std::fs::remove_dir_all(dir.join("gone")).unwrap();
    write(dir, "gone", "now a file\n");
    std::fs::set_permissions(dir.join("bin/run"), std::fs::Permissions::from_mode(0o644)).unwrap();
    commit(dir, "edit, delete, retype");

    git(dir, &["checkout", "-q", "-b", "side"]);
    write(dir, "side.txt", "from the side\n");
    commit(dir, "side work");
    git(dir, &["checkout", "-q", "main"]);
    std::fs::remove_file(dir.join("src/lib.rs")).unwrap();
    write(dir, "src/lib.rs/mod.rs", "// a file became a directory\n");
    commit(dir, "file to directory");
    git(dir, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
}
