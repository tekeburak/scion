//! The `scn` binary against an in-process `scion-server` on a memory store.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use bytes::Bytes;
use scion_derived::test_util::{REPO, changeset};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{BlobId, ChangesetId, FileChange, FileType, RepoPath};
use tokio::net::TcpListener;

fn tracked(content: &[u8]) -> FileChange {
    FileChange::Tracked {
        content: BlobId::hash(content),
        kind: FileType::Regular,
        size: content.len() as u64,
        copy_from: None,
    }
}

/// A server whose `main` is `c1` on top of `c0`; returns its address and both
/// changesets.
async fn serve() -> (String, ChangesetId, ChangesetId) {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let mut c0 = BTreeMap::new();
    for (path, content) in [
        ("README", &b"one\ntwo\n"[..]),
        ("srv/api/main.rs", b"fn main() {}\n"),
        ("srv/web/index.html", b"<html></html>\n"),
    ] {
        store
            .put_blob(Bytes::copy_from_slice(content))
            .await
            .unwrap();
        c0.insert(RepoPath::new(path).unwrap(), tracked(content));
    }
    let c0 = store
        .put_changeset(&changeset(vec![], c0, "initial import"))
        .await
        .unwrap();
    let readme = b"one\ntwo\nthree\n";
    store.put_blob(Bytes::from_static(readme)).await.unwrap();
    let c1 = store
        .put_changeset(&changeset(
            vec![c0],
            BTreeMap::from([(RepoPath::new("README").unwrap(), tracked(readme))]),
            "add a third line",
        ))
        .await
        .unwrap();
    store.create_ref(REPO, "main", c1).await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(scion_server::serve(listener, store.clone(), store));
    (format!("http://{addr}"), c0, c1)
}

/// Run `scn args` in `dir`; panics unless it succeeds. Returns stdout.
fn scn(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_scn"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "scn {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_commands_end_to_end() {
    let (server, c0, c1) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let short = |id: ChangesetId| id.to_hex()[..12].to_owned();

    // Sparse clone: srv/web is never fetched.
    let out = scn(
        dir.path(),
        &[
            "clone", &server, REPO, "wc", "--sparse", "README", "--sparse", "srv/api/",
        ],
    );
    assert!(out.contains(&format!("at {c1}")), "{out}");
    let wc = dir.path().join("wc");
    assert!(wc.join("srv/api/main.rs").is_file());
    assert!(!wc.join("srv/web").exists());

    assert_eq!(scn(&wc, &["status"]), "");

    // History, from the root and from a subdirectory.
    let log = scn(&wc, &["log"]);
    let first = log.find(&format!("changeset {c1}")).expect("c1 listed");
    let second = log.find(&format!("changeset {c0}")).expect("c0 listed");
    assert!(first < second, "newest first: {log}");
    assert!(log.contains("    add a third line"), "{log}");
    assert!(log.contains("Date:   1970-01-01 00:00:00 +0000"), "{log}");
    let api_log = scn(&wc.join("srv"), &["log", "api"]);
    assert!(api_log.contains(&c0.to_hex()) && !api_log.contains(&c1.to_hex()));
    assert_eq!(
        scn(&wc, &["log", "-n", "1"]).matches("changeset ").count(),
        1
    );

    // Diff between revisions, by ref and by id.
    let diff = scn(&wc, &["diff", &c0.to_hex(), "main"]);
    assert!(diff.starts_with("diff a/README b/README\n"), "{diff}");
    assert!(diff.contains("--- a/README\n+++ b/README\n"), "{diff}");
    assert!(diff.contains("+three\n"), "{diff}");

    // Blame: the first two lines are c0's, the third c1's.
    assert_eq!(
        scn(&wc, &["blame", "README"]),
        format!("{0} one\n{0} two\n{1} three\n", short(c0), short(c1))
    );
    assert_eq!(
        scn(&wc, &["blame", "README", "-r", &c0.to_hex()]),
        format!("{0} one\n{0} two\n", short(c0))
    );

    // Local edits show in status and diff; untracked files only in status.
    std::fs::write(wc.join("README"), b"one\n2\nthree\n").unwrap();
    std::fs::remove_file(wc.join("srv/api/main.rs")).unwrap();
    std::fs::write(wc.join("notes.txt"), b"scratch\n").unwrap();
    assert_eq!(
        scn(&wc, &["status"]),
        "M README\n? notes.txt\nD srv/api/main.rs\n"
    );
    let diff = scn(&wc, &["diff"]);
    assert!(diff.contains("-two\n+2\n"), "{diff}");
    assert!(
        diff.contains("deleted regular file\n--- a/srv/api/main.rs\n+++ /dev/null\n"),
        "{diff}"
    );
    assert!(!diff.contains("notes.txt"), "{diff}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_are_reported() {
    let (server, ..) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_scn"))
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap()
    };
    let out = run(&["status"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not inside a working copy"));

    scn(dir.path(), &["clone", &server, REPO, "wc"]);
    let wc = dir.path().join("wc");
    let out = Command::new(env!("CARGO_BIN_EXE_scn"))
        .current_dir(&wc)
        .args(["blame", "srv"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "a directory has no blame");
    let out = Command::new(env!("CARGO_BIN_EXE_scn"))
        .current_dir(&wc)
        .args(["log", "-r", "no-such-label"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

/// Run `scn args` in `dir` as author `Ada`; returns (success, stdout, stderr).
fn scn_try(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_scn"))
        .current_dir(dir)
        .env("SCN_AUTHOR", "Ada <ada@example.com>")
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// The changeset id at the end of `committed <id>` / `cloned … at <id> …`.
fn id_after(out: &str, marker: &str) -> ChangesetId {
    let rest = &out[out.find(marker).expect(marker) + marker.len()..];
    rest[..64].parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_commit_land_end_to_end() {
    let (server, _, c1) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    scn(dir.path(), &["clone", &server, REPO, "a"]);
    scn(dir.path(), &["clone", &server, REPO, "b"]);
    let a = dir.path().join("a");
    let b = dir.path().join("b");

    let (ok, _, err) = scn_try(&a, &["commit", "-m", "empty"]);
    assert!(!ok && err.contains("nothing to commit"), "{err}");

    // Edit, delete, create; only added files join the commit.
    std::fs::write(a.join("README"), b"one\ntwo\nthree\nfour\n").unwrap();
    std::fs::remove_file(a.join("srv/web/index.html")).unwrap();
    std::fs::create_dir_all(a.join("docs")).unwrap();
    std::fs::write(a.join("docs/guide.md"), b"# guide\n").unwrap();
    std::fs::write(a.join("scratch.txt"), b"not for commit\n").unwrap();
    assert_eq!(scn(&a.join("docs"), &["add", "."]), "A docs/guide.md\n");
    assert_eq!(
        scn(&a, &["status"]),
        "M README\nA docs/guide.md\n? scratch.txt\nD srv/web/index.html\n"
    );
    let diff = scn(&a, &["diff"]);
    assert!(diff.contains("new regular file\n--- /dev/null\n+++ b/docs/guide.md\n"));
    let (ok, _, err) = scn_try(&a, &["add", "nope"]);
    assert!(!ok && err.contains("no such file"), "{err}");

    let (ok, out, err) = scn_try(&a, &["commit", "-m", "extend the readme"]);
    assert!(ok, "{err}");
    let c2 = id_after(&out, "committed ");
    assert_eq!(scn(&a, &["status"]), "? scratch.txt\n");

    // The commit is an anonymous head: main has not moved.
    let log = scn(&a, &["log", "-n", "2"]);
    assert!(log.contains(&format!("changeset {c2}")), "{log}");
    assert!(log.contains("Author: Ada <ada@example.com>"), "{log}");
    assert!(scn(&a, &["log", "-r", "main", "-n", "1"]).contains(&c1.to_hex()));
    assert!(scn(&a, &["blame", "README"]).ends_with(&format!("{} four\n", &c2.to_hex()[..12])));

    assert_eq!(scn(&a, &["land"]), format!("landed {c2} on main\n"));
    assert_eq!(
        scn(&a, &["land"]),
        format!("nothing to land: main already has the changes of {c2}\n")
    );
    assert!(scn(&b, &["log", "-r", "main", "-n", "1"]).contains(&c2.to_hex()));

    // b still sits on c1 and rewrites the line a extended: a conflict.
    std::fs::write(b.join("README"), b"diverged\n").unwrap();
    let (ok, out, err) = scn_try(&b, &["commit", "-m", "diverge"]);
    assert!(ok, "{err}");
    let c3 = id_after(&out, "committed ");
    let (ok, _, err) = scn_try(&b, &["land"]);
    assert!(
        !ok && err.contains("cannot land")
            && err.contains("conflicts with main")
            && err.contains("\n  README\n")
            && err.contains("scn update"),
        "{err}"
    );
    assert!(!scn(&b, &["log", "-r", "main", "-n", "1"]).contains(&c3.to_hex()));

    // A fresh clone of main sees the landed tree.
    let out = scn(dir.path(), &["clone", &server, REPO, "c"]);
    assert_eq!(id_after(&out, "at "), c2);
    let c = dir.path().join("c");
    assert_eq!(
        std::fs::read(c.join("docs/guide.md")).unwrap(),
        b"# guide\n"
    );
    assert!(!c.join("srv/web/index.html").exists());
    assert!(!c.join("scratch.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_needs_an_author() {
    let (server, ..) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    scn(dir.path(), &["clone", &server, REPO, "wc"]);
    let wc = dir.path().join("wc");
    std::fs::write(wc.join("README"), b"x\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_scn"))
        .current_dir(&wc)
        .env_remove("SCN_AUTHOR")
        .args(["commit", "-m", "m"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--author"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_clone_commit_land_without_git() {
    let store = Arc::new(MemoryStore::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(scion_server::serve(listener, store.clone(), store));
    let dir = tempfile::tempdir().unwrap();

    let out = scn(dir.path(), &["init", &server, "fresh"]);
    assert!(
        out.starts_with(&format!("created fresh on {server} (main at ")),
        "{out}"
    );
    let (ok, _, err) = scn_try(dir.path(), &["init", &server, "fresh"]);
    assert!(!ok && err.contains("fresh already exists"), "{err}");

    let out = scn(dir.path(), &["clone", &server, "fresh"]);
    assert!(out.ends_with("(0 files)\n"), "{out}");
    let wc = dir.path().join("fresh");
    assert_eq!(scn(&wc, &["status"]), "");

    std::fs::write(wc.join("hello.txt"), b"hello\n").unwrap();
    scn(&wc, &["add", "hello.txt"]);
    let (ok, out, err) = scn_try(&wc, &["commit", "-m", "first file"]);
    assert!(ok, "{err}");
    let c1 = id_after(&out, "committed ");
    assert!(scn(&wc, &["land"]).starts_with(&format!("landed {c1} on main")));
    let out = scn(dir.path(), &["clone", &server, "fresh", "again"]);
    assert_eq!(id_after(&out, "at "), c1);
    assert_eq!(
        std::fs::read(dir.path().join("again/hello.txt")).unwrap(),
        b"hello\n"
    );

    // A misspelled name reaches no repository.
    let (ok, _, err) = scn_try(dir.path(), &["clone", &server, "frehs"]);
    assert!(!ok && err.contains("frehs"), "{err}");
}

/// Whether every changeset `scn log -r main` lists has one parent at most.
fn main_is_linear(wc: &Path) -> bool {
    !scn(wc, &["log", "-r", "main", "-n", "1000"]).contains("Parents:")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clones_land_alternately_with_update() {
    let (server, c0, _) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    scn(dir.path(), &["clone", &server, REPO, "a"]);
    scn(dir.path(), &["clone", &server, REPO, "b"]);
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));

    for round in 0..3 {
        // Both commit on whatever they last saw and land at once, with no
        // update in between; the server rebases whichever comes second.
        for (wc, who) in [(&a, "a"), (&b, "b")] {
            std::fs::write(wc.join(format!("{who}{round}.txt")), format!("{round}\n")).unwrap();
            scn(wc, &["add", "."]);
            let (ok, _, err) = scn_try(wc, &["commit", "-m", &format!("{who} {round}")]);
            assert!(ok, "{err}");
        }
        let (la, lb) = std::thread::scope(|t| {
            let la = t.spawn(|| scn(&a, &["land"]));
            let lb = t.spawn(|| scn(&b, &["land"]));
            (la.join().unwrap(), lb.join().unwrap())
        });
        assert!(
            la.starts_with("landed ") && lb.starts_with("landed "),
            "{la}{lb}"
        );
        // A rebased one now sits on its landed copy, which has the other's
        // file whichever landed first.
        for (wc, out) in [(&a, &la), (&b, &lb)] {
            if let Some((_, landed)) = out.trim_end().split_once(" as ") {
                let head = id_after(&scn(wc, &["log", "-n", "1"]), "changeset ");
                assert_eq!(head.to_hex(), landed);
            }
            assert_eq!(scn(wc, &["status"]), "");
        }
        assert!(la.contains(" as ") || lb.contains(" as "), "{la}{lb}");
        assert!(main_is_linear(&b));
    }
    // A fresh clone of main has every round of both.
    scn(dir.path(), &["clone", &server, REPO, "c"]);
    for round in 0..3 {
        for who in ["a", "b"] {
            assert!(dir.path().join(format!("c/{who}{round}.txt")).is_file());
        }
    }
    // Whichever stayed behind catches up without anything to replay.
    scn(&a, &["update"]);
    scn(&b, &["update"]);
    assert!(a.join("b2.txt").is_file() && b.join("a2.txt").is_file());
    assert_eq!(scn(&a, &["update"]).trim_end(), {
        let head = id_after(&scn(&a, &["log", "-n", "1"]), "changeset ");
        format!("{head} already contains main")
    });

    // Local edits are refused; a conflict changes nothing.
    std::fs::write(a.join("README"), b"one\nA\nthree\n").unwrap();
    let (ok, _, err) = scn_try(&a, &["update"]);
    assert!(
        !ok && err.contains("local changes") && err.contains("README"),
        "{err}"
    );
    let (ok, _, err) = scn_try(&a, &["commit", "-m", "a readme"]);
    assert!(ok, "{err}");
    std::fs::write(b.join("README"), b"one\nB\nthree\n").unwrap();
    let (ok, out, err) = scn_try(&b, &["commit", "-m", "b readme"]);
    assert!(ok, "{err}");
    let b_head = id_after(&out, "committed ");
    scn(&a, &["land"]);
    let (ok, _, err) = scn_try(&b, &["update"]);
    assert!(
        !ok && err.contains("conflicts") && err.contains("\n  README"),
        "{err}"
    );
    assert_eq!(std::fs::read(b.join("README")).unwrap(), b"one\nB\nthree\n");
    assert_eq!(
        id_after(&scn(&b, &["log", "-n", "1"]), "changeset "),
        b_head,
        "the head did not move"
    );

    // checkout: refused when dirty, forced, and back.
    std::fs::write(b.join("b0.txt"), b"edited\n").unwrap();
    let (ok, _, err) = scn_try(&b, &["checkout", &c0.to_hex()]);
    assert!(!ok && err.contains("b0.txt"), "{err}");
    scn(&b, &["checkout", "--force", &c0.to_hex()]);
    assert!(!b.join("b0.txt").exists() && !b.join("a0.txt").exists());
    assert_eq!(std::fs::read(b.join("README")).unwrap(), b"one\ntwo\n");
    assert_eq!(scn(&b, &["status"]), "");
    scn(&b, &["checkout", &b_head.to_hex()]);
    assert_eq!(std::fs::read(b.join("b0.txt")).unwrap(), b"0\n");
    assert_eq!(std::fs::read(b.join("README")).unwrap(), b"one\nB\nthree\n");
    assert_eq!(scn(&b, &["status"]), "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn land_without_waiting_and_read_the_request_back() {
    let (server, ..) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    scn(dir.path(), &["clone", &server, REPO, "a"]);
    let a = dir.path().join("a");
    std::fs::write(a.join("late.txt"), b"late\n").unwrap();
    scn(&a, &["add", "late.txt"]);
    let (ok, out, err) = scn_try(&a, &["commit", "-m", "late"]);
    assert!(ok, "{err}");
    let late = id_after(&out, "committed ");
    let out = scn(&a, &["land", "--no-wait"]);
    let id = out
        .strip_prefix("submitted request ")
        .and_then(|r| r.split_once(' '))
        .expect(&out)
        .0
        .to_owned();
    assert!(out.ends_with(&format!("scn land --status {id}\n")), "{out}");
    let status = loop {
        let st = scn(&a, &["land", "--status", &id]);
        if !st.contains(": pending,") && !st.contains(": landing,") {
            break st;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(status, format!("request {id}: landed {late} on main\n"));
    let (ok, _, err) = scn_try(&a, &["land", "--status", "999999"]);
    assert!(!ok && err.contains("999999"), "{err}");
}
