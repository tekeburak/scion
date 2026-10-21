//! Every `scn --json` output against its schema in `schemas/v1`, on an
//! in-process `scion-server` over a memory store. `login` is checked in
//! `login.rs`, next to its fake GitHub.

mod schema;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use bytes::Bytes;
use scion_derived::test_util::{REPO, changeset};
use scion_store::{MemoryStore, MetadataStore, ObjectStore};
use scion_types::{BlobId, FileChange, FileType, RepoPath};
use serde_json::Value;
use tokio::net::TcpListener;

fn key() -> scion_server::TokenKey {
    scion_server::TokenKey::from_hex(&"5c".repeat(32)).unwrap()
}

/// A token for `sp:test`, an admin of the server and of [`REPO`].
fn token() -> String {
    let hour = std::time::Duration::from_secs(3600);
    scion_server::token(&key(), "sp:test", hour, std::time::SystemTime::now()).unwrap()
}

fn tracked(content: &[u8]) -> FileChange {
    FileChange::Tracked {
        content: BlobId::hash(content),
        kind: FileType::Regular,
        size: content.len() as u64,
        copy_from: None,
    }
}

/// A server whose `main` is `README` in one changeset and whose `release/1`
/// is the same changeset.
async fn serve() -> String {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let readme = b"one\ntwo\nthree\n";
    store.put_blob(Bytes::from_static(readme)).await.unwrap();
    let c0 = store
        .put_changeset(&changeset(
            vec![],
            BTreeMap::from([(RepoPath::new("README").unwrap(), tracked(readme))]),
            "initial import",
        ))
        .await
        .unwrap();
    store.create_ref(REPO, "main", c0).await.unwrap();
    store.create_ref(REPO, "release/1", c0).await.unwrap();
    let admin = scion_store::Grant {
        principal: "sp:test".into(),
        path_prefix: String::new(),
        role: scion_store::Role::Admin,
    };
    for scope in [scion_store::SERVER_GRANTS, REPO] {
        store.put_grant(scope, &admin).await.unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(scion_server::serve(
        listener,
        store.clone(),
        store,
        key(),
        None,
    ));
    format!("http://{addr}")
}

/// Runs `scn --json` and checks each output against its schema, keeping the
/// names of the schemas it checked.
struct Scn {
    checked: BTreeSet<String>,
}

impl Scn {
    /// `scn --json args` in `dir`; returns (exit code, stdout).
    fn run(dir: &Path, args: &[&str]) -> (i32, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_scn"))
            .env("SCN_TOKEN", token())
            .env("SCN_AUTHOR", "Ada <ada@example.com>")
            .current_dir(dir)
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8(out.stdout).unwrap(),
        )
    }

    /// Succeeds and matches `schemas/v1/<name>.json`.
    fn ok(&mut self, name: &str, dir: &Path, args: &[&str]) -> Value {
        let (code, out) = Self::run(dir, args);
        assert_eq!(code, 0, "scn --json {args:?}: {out}");
        self.checked.insert(name.to_owned());
        schema::check(name, &out)
    }

    /// Fails with `exit` and `error.json` naming `code`; returns its paths.
    fn fails(&mut self, code: &str, exit: i32, dir: &Path, args: &[&str]) -> Vec<String> {
        let (got, out) = Self::run(dir, args);
        assert_eq!(got, exit, "scn --json {args:?}: {out}");
        self.checked.insert("error".to_owned());
        let v = schema::check("error", &out);
        assert_eq!(v["error"]["code"], code, "{v}");
        let paths = v["error"]["paths"].as_array().unwrap();
        paths
            .iter()
            .map(|p| p.as_str().unwrap().to_owned())
            .collect()
    }
}

fn commit(scn: &mut Scn, wc: &Path, message: &str) -> String {
    let v = scn.ok("commit", wc, &["commit", "-m", message]);
    v["head"].as_str().unwrap().to_owned()
}

/// In `b`, whose README edit conflicts with `landed` on `main`: the conflict
/// explained, refused, then resolved with markers and landed.
fn conflict(scn: &mut Scn, b: &Path, landed: &str) {
    std::fs::write(b.join("README"), b"one\nB\nthree\n").unwrap();
    let b_head = commit(scn, b, "b");
    let v = scn.ok("explain-conflict", b, &["explain-conflict"]);
    assert_eq!(v["changeset"], b_head.as_str(), "{v}");
    let readme = &v["conflicts"][0];
    assert_eq!(readme["path"], "README", "{v}");
    assert_eq!(readme["ours"][0]["id"], b_head.as_str(), "{v}");
    assert_eq!(readme["theirs"][0]["subject"], "a", "{v}");
    assert_eq!(readme["theirs"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(scn.fails("conflict", 1, b, &["update"]), ["README"]);
    assert_eq!(scn.fails("needs_rebase", 1, b, &["land"]), ["README"]);
    let v = scn.ok("update", b, &["update", "--conflicts"]);
    assert_eq!(v["conflicts"][0], "README", "{v}");
    assert_eq!(v["head"], landed, "{v}");
    assert_eq!(
        scn.fails("unresolved_conflicts", 1, b, &["commit", "-m", "b"]),
        ["README"]
    );
    std::fs::write(b.join("README"), b"one\nA and B\nthree\n").unwrap();
    commit(scn, b, "b, on a");
    let v = scn.ok("explain-conflict", b, &["explain-conflict"]);
    assert!(
        v["changeset"].is_null() && v["conflicts"] == Value::Array(vec![]),
        "{v}"
    );
    let v = scn.ok("land", b, &["land", "--no-wait"]);
    assert!(v["request"]["id"].is_u64(), "{v}");
}

/// Grant, list and revoke in the working copy `a`.
fn access(scn: &mut Scn, a: &Path) {
    let v = scn.ok(
        "access-grant",
        a,
        &["access", "grant", "github:octo", "writer", "--path", "docs"],
    );
    assert_eq!(v["path"], "docs");
    let v = scn.ok("access-list", a, &["access", "list"]);
    assert_eq!(v["grants"].as_array().unwrap().len(), 2, "{v}");
    scn.ok(
        "access-revoke",
        a,
        &["access", "revoke", "github:octo", "--path", "docs"],
    );
    scn.fails(
        "invalid_argument",
        1,
        a,
        &["access", "grant", "anyone", "writer"],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_json_output_matches_its_schema() {
    let server = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    let mut scn = Scn {
        checked: BTreeSet::new(),
    };

    let v = scn.ok("init", top, &["init", &server, "agents"]);
    assert_eq!(v["repo"], "agents");
    scn.fails("already_exists", 1, top, &["init", &server, "agents"]);
    scn.fails("not_a_working_copy", 1, top, &["status"]);
    scn.fails("usage", 2, top, &["log", "--no-such-flag"]);
    scn.ok("whoami", top, &["whoami", &server]);

    let v = scn.ok("clone", top, &["clone", &server, REPO, "a"]);
    assert_eq!(v["files"], 1);
    scn.ok("clone", top, &["clone", &server, REPO, "b"]);
    let (a, b) = (top.join("a"), top.join("b"));
    let v = scn.ok("whoami", &a, &["whoami"]);
    assert_eq!(v["grants"][0]["role"], "admin", "{v}");

    // Reading.
    let v = scn.ok("log", &a, &["log", "--landed"]);
    assert_eq!(v["changesets"][0]["message"], "initial import");
    scn.ok("log", &a, &["log", "README", "-n", "1"]);
    let v = scn.ok("blame", &a, &["blame", "README"]);
    assert_eq!(v["lines"][2]["line"], "three", "{v}");

    // Writing and landing.
    std::fs::write(a.join("README"), b"one\nA\nthree\n").unwrap();
    std::fs::write(a.join("a.txt"), b"a\n").unwrap();
    let v = scn.ok("status", &a, &["status"]);
    assert_eq!(v["files"][1]["status"], "untracked", "{v}");
    let v = scn.ok("add", &a, &["add", "a.txt"]);
    assert_eq!(v["added"][0], "a.txt");
    let v = scn.ok("diff", &a, &["diff"]);
    assert!(
        v["files"][0]["patch"].as_str().unwrap().contains("+A\n"),
        "{v}"
    );
    let a_head = commit(&mut scn, &a, "a");
    let v = scn.ok("land", &a, &["land"]);
    assert_eq!(v["request"]["status"], "landed", "{v}");
    let landed = v["head"].as_str().unwrap().to_owned();
    let id = v["request"]["id"].to_string();
    scn.ok("land", &a, &["land", "--status", &id]);
    scn.ok("diff", &a, &["diff", "release/1", "main"]);
    let v = scn.ok("requests", &a, &["requests"]);
    assert_eq!(v["requests"][0]["landed"], landed.as_str(), "{v}");
    let v = scn.ok("log", &a, &["log", "--landed", "-n", "1"]);
    assert_eq!(v["changesets"][0]["landed_by"]["status"], "landed", "{v}");
    scn.fails("nothing_to_commit", 1, &a, &["commit", "-m", "empty"]);

    conflict(&mut scn, &b, &landed);

    // Moving the head around.
    std::fs::write(a.join("README"), b"local edit\n").unwrap();
    assert_eq!(scn.fails("dirty", 1, &a, &["checkout", "main"]), ["README"]);
    scn.ok("checkout", &a, &["checkout", "--force", &a_head]);
    let v = scn.ok("update", &a, &["update"]);
    assert_eq!(v["previous"], a_head.as_str(), "{v}");
    let v = scn.ok("merge", &a, &["merge", "release/1"]);
    assert_eq!(
        v["head"], v["previous"],
        "main already contains release/1: {v}"
    );
    std::fs::write(a.join("a.txt"), b"a, again\n").unwrap();
    let again = commit(&mut scn, &a, "a again");
    let v = scn.ok("revert", &a, &["revert", &again]);
    assert_eq!(v["reverted"], again.as_str(), "{v}");

    access(&mut scn, &a);

    // Every schema but the shared definitions and `login` was used.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/v1");
    let all: BTreeSet<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            e.unwrap()
                .path()
                .file_stem()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .filter(|n| n != "defs" && n != "login")
        .collect();
    assert_eq!(scn.checked, all);
}

#[test]
fn the_check_rejects_what_the_schema_does_not_allow() {
    for bad in [
        r#"{"head": "abc"}"#,
        r#"{"head": null}"#,
        "{}",
        &format!(r#"{{"head": "{}", "more": 1}}"#, "a".repeat(64)),
    ] {
        let failed = std::panic::catch_unwind(|| schema::check("checkout", bad)).is_err();
        assert!(failed, "{bad}");
    }
    schema::check("checkout", &format!(r#"{{"head": "{}"}}"#, "a".repeat(64)));
}
