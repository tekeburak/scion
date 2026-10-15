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
    let store = Arc::new(MemoryStore::new());
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
