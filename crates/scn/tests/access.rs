//! `scn access` and `scn whoami` against an in-process `scion-server` on a
//! memory store: an admin shares a directory with a second principal, who
//! lands under it until the grant is revoked.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use scion_store::{Grant, MemoryStore, MetadataStore, Role, SERVER_GRANTS};
use tokio::net::TcpListener;

fn key() -> scion_server::TokenKey {
    scion_server::TokenKey::from_hex(&"5c".repeat(32)).unwrap()
}

/// A token of [`key`] for `principal`, valid for an hour.
fn token(principal: &str) -> String {
    let hour = Duration::from_secs(3600);
    scion_server::token(&key(), principal, hour, SystemTime::now()).unwrap()
}

/// A server where `sp:admin` may create repositories; returns its address.
async fn serve() -> String {
    let store = Arc::new(MemoryStore::new());
    let admin = Grant {
        principal: "sp:admin".into(),
        path_prefix: String::new(),
        role: Role::Admin,
    };
    store.put_grant(SERVER_GRANTS, &admin).await.unwrap();
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

/// Run `scn args` in `dir` as `principal`; returns whether it succeeded,
/// stdout and stderr.
fn scn_as(principal: &str, dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_scn"))
        .env("SCN_TOKEN", token(principal))
        .env("SCN_AUTHOR", "Access <access@example.com>")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// [`scn_as`] that panics unless it succeeds; returns stdout.
fn scn(principal: &str, dir: &Path, args: &[&str]) -> String {
    let (ok, out, err) = scn_as(principal, dir, args);
    assert!(ok, "scn {args:?} as {principal} failed: {err}");
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admin_shares_a_directory_and_takes_it_back() {
    let server = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let (admin, dev) = ("sp:admin", "sp:dev");

    scn(admin, dir.path(), &["init", &server, "app"]);
    scn(admin, dir.path(), &["clone", &server, "app", "a"]);
    let a = dir.path().join("a");
    std::fs::create_dir_all(a.join("lib")).unwrap();
    std::fs::write(a.join("lib/x.rs"), b"x\n").unwrap();
    std::fs::write(a.join("README"), b"app\n").unwrap();
    scn(admin, &a, &["add", "."]);
    scn(admin, &a, &["commit", "-m", "seed"]);
    scn(admin, &a, &["land"]);

    let out = scn(admin, &a, &["whoami"]);
    assert!(
        out.starts_with(&format!("sp:admin on {server}, token valid until ")),
        "{out}"
    );
    assert!(out.ends_with("\nadmin on app\n"), "{out}");

    // Nothing granted yet: the refusal says who can fix it, and how.
    let (ok, _, err) = scn_as(dev, dir.path(), &["clone", &server, "app", "b"]);
    assert!(!ok);
    assert!(
        err.contains("sp:dev is not a reader of")
            && err.contains("`scn access grant sp:dev reader`"),
        "{err}"
    );

    let out = scn(
        admin,
        &a,
        &["access", "grant", dev, "writer", "--path", "lib/"],
    );
    assert_eq!(out, "granted sp:dev writer on app:lib/\n");
    scn(admin, &a, &["access", "grant", dev, "reader"]);
    assert_eq!(
        scn(admin, &a, &["access", "list"]),
        "sp:admin admin on app\nsp:dev reader on app\nsp:dev writer on app:lib/\n"
    );

    scn(dev, dir.path(), &["clone", &server, "app", "b"]);
    let b = dir.path().join("b");
    let out = scn(dev, &b, &["whoami"]);
    assert!(
        out.ends_with("\nreader on app\nwriter on app:lib/\n"),
        "{out}"
    );
    std::fs::write(b.join("lib/y.rs"), b"y\n").unwrap();
    scn(dev, &b, &["add", "lib"]);
    scn(dev, &b, &["commit", "-m", "lib: y"]);
    assert!(scn(dev, &b, &["land"]).starts_with("landed "));

    // Outside its directory, and managing access, are refused.
    std::fs::write(b.join("README"), b"dev\n").unwrap();
    let (ok, _, err) = scn_as(dev, &b, &["commit", "-m", "readme"]);
    assert!(!ok);
    assert!(err.contains("sp:dev may not change README"), "{err}");
    let (ok, _, err) = scn_as(dev, &b, &["access", "list"]);
    assert!(!ok);
    assert_eq!(
        err,
        "scn: sp:dev is not an admin of \"app\"; an admin of it can run \
         `scn access grant sp:dev admin`\n"
    );
    scn(dev, &b, &["checkout", "--force", "@"]);

    // A change committed before the revoke no longer lands after it.
    std::fs::write(b.join("lib/z.rs"), b"z\n").unwrap();
    scn(dev, &b, &["add", "lib"]);
    scn(dev, &b, &["commit", "-m", "lib: z"]);
    let out = scn(admin, &a, &["access", "revoke", dev, "--path", "lib"]);
    assert_eq!(out, "revoked sp:dev on app:lib/\n");
    let (ok, _, err) = scn_as(dev, &b, &["land"]);
    assert!(!ok);
    assert!(err.contains("sp:dev is not a writer of"), "{err}");

    // Bad input is refused by the server with its reason.
    for (args, says) in [
        (
            &["access", "revoke", dev, "--path", "lib"][..],
            "sp:dev has no grant on lib/ in \"app\"",
        ),
        (&["access", "grant", dev, "owner"], "unknown role \"owner\""),
        (
            &["access", "grant", "dev", "reader"],
            "github:<login> or sp:<name>",
        ),
        (
            &["access", "grant", dev, "reader", "--path", "../x"],
            "--path",
        ),
    ] {
        let (ok, _, err) = scn_as(admin, &a, args);
        assert!(!ok, "{args:?}");
        assert!(err.contains(says), "{args:?}: {err}");
    }

    // Outside a working copy, `whoami` names the server.
    let out = scn(dev, dir.path(), &["whoami", &server]);
    assert!(
        out.starts_with(&format!("sp:dev on {server}, token valid until ")),
        "{out}"
    );
    assert_eq!(out.lines().count(), 1, "{out}");
}
