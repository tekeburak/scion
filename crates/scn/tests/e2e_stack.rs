//! End to end on the compose stack: `scn` against `scion-server` backed by a
//! real Postgres and a real S3-compatible object store.
//!
//! Requires `--features integration` and the services from
//! `deploy/docker-compose.yml` (bucket created). Override the defaults with
//! `SCION_TEST_DATABASE_URL` and `SCION_TEST_S3_{ENDPOINT,BUCKET,ACCESS_KEY,SECRET_KEY}`.
//! The repository is created with `scn init` and seeded through a working
//! copy, with no git repository involved; two clones then land alternately
//! with `scn update` between, and a `release/1` fix is merged and landed.
//! Each run uses its own repository name, so runs never see each other's
//! refs or changesets even though they share the database and bucket.

#![cfg(feature = "integration")]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use object_store::aws::AmazonS3Builder;
use scion_store::{ObjectStorage, PostgresStore};
use scion_types::ChangesetId;
use tokio::net::TcpListener;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// `scion-server` on the compose stack, holding no repository of its own.
/// Returns its address.
async fn serve() -> String {
    let meta = PostgresStore::connect(&env_or(
        "SCION_TEST_DATABASE_URL",
        "postgres://scion:scion@localhost:5432/scion",
    ))
    .await
    .expect("connect to postgres");
    meta.migrate().await.unwrap();
    let s3 = AmazonS3Builder::new()
        .with_endpoint(env_or("SCION_TEST_S3_ENDPOINT", "http://localhost:9000"))
        .with_bucket_name(env_or("SCION_TEST_S3_BUCKET", "scion-objects"))
        .with_access_key_id(env_or("SCION_TEST_S3_ACCESS_KEY", "scion"))
        .with_secret_access_key(env_or("SCION_TEST_S3_SECRET_KEY", "scionscion"))
        .with_region("us-east-1")
        .with_allow_http(true)
        .build()
        .expect("valid S3 configuration");
    let objects = ObjectStorage::new(Arc::new(s3));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(scion_server::serve(
        listener,
        Arc::new(meta),
        Arc::new(objects),
    ));
    format!("http://{addr}")
}

/// Run `scn args` in `dir`; panics unless it succeeds. Returns stdout.
fn scn(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_scn"))
        .current_dir(dir)
        .env("SCN_AUTHOR", "E2E <e2e@example.com>")
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

/// The id in `committed <id>`.
fn committed(out: &str) -> ChangesetId {
    out.trim()
        .strip_prefix("committed ")
        .expect(out)
        .parse()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_clone_commit_land_on_postgres_and_s3() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let repo = format!("e2e-{}-{nanos}", std::process::id());
    let server = serve().await;
    let dir = tempfile::tempdir().unwrap();

    // A new repository, seeded through a working copy: no git involved.
    let out = scn(dir.path(), &["init", &server, &repo]);
    assert!(
        out.starts_with(&format!("created {repo} on {server}")),
        "{out}"
    );
    let out = scn(dir.path(), &["clone", &server, &repo, "seed"]);
    assert!(out.ends_with("(0 files)\n"), "{out}");
    let seed = dir.path().join("seed");
    for (path, content) in [
        ("README", &b"one\ntwo\n"[..]),
        ("srv/api/main.rs", b"fn main() {}\n"),
        ("srv/web/index.html", b"<html></html>\n"),
    ] {
        std::fs::create_dir_all(seed.join(path).parent().unwrap()).unwrap();
        std::fs::write(seed.join(path), content).unwrap();
    }
    scn(&seed, &["add", "."]);
    let root = committed(&scn(&seed, &["commit", "-m", "seed"]));
    assert!(scn(&seed, &["land"]).starts_with(&format!("landed {root} on main")));

    // A misspelled name reaches no repository, and a second `init` is refused.
    let typo = format!("{repo}x");
    for args in [
        &["clone", &server, &typo, "typo"][..],
        &["init", &server, &repo],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_scn"))
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(!out.status.success(), "scn {args:?} succeeded");
    }

    // Sparse clone: only srv/api and README are fetched.
    let out = scn(
        dir.path(),
        &[
            "clone", &server, &repo, "a", "--sparse", "srv/api", "--sparse", "README",
        ],
    );
    assert!(out.contains(&format!("at {root}")), "{out}");
    let a = dir.path().join("a");
    assert!(!a.join("srv/web").exists());

    // Edit, add, commit, land.
    std::fs::write(a.join("README"), b"one\ntwo\nthree\n").unwrap();
    std::fs::write(a.join("srv/api/lib.rs"), b"pub fn api() {}\n").unwrap();
    scn(&a, &["add", "srv/api/lib.rs"]);
    let head = committed(&scn(&a, &["commit", "-m", "grow the api"]));
    assert_eq!(scn(&a, &["status"]), "");
    assert!(scn(&a, &["land"]).starts_with(&format!("landed {head} on main")));

    // History and blame come from the server's derived data.
    let log = scn(&a, &["log", "-r", "main"]);
    let (first, second) = (
        log.find(&head.to_hex()).expect("head in log"),
        log.find(&root.to_hex()).expect("root in log"),
    );
    assert!(first < second, "{log}");
    assert_eq!(
        scn(&a, &["blame", "README"]),
        format!(
            "{0} one\n{0} two\n{1} three\n",
            &root.to_hex()[..12],
            &head.to_hex()[..12]
        )
    );
    let diff = scn(&a, &["diff", &root.to_hex(), "main"]);
    assert!(
        diff.contains("+three\n") && diff.contains("+++ b/srv/api/lib.rs\n"),
        "{diff}"
    );

    // A fresh full clone sees the landed tree, read back from S3.
    scn(dir.path(), &["clone", &server, &repo, "b"]);
    let b = dir.path().join("b");
    assert_eq!(
        std::fs::read(b.join("README")).unwrap(),
        b"one\ntwo\nthree\n"
    );
    assert_eq!(
        std::fs::read(b.join("srv/api/lib.rs")).unwrap(),
        b"pub fn api() {}\n"
    );
    assert!(b.join("srv/web/index.html").is_file());

    two_clones_land_at_once(&a, &b);
    merge_a_release_fix(&server, &repo, &a, &b).await;
}

/// `b` commits a fix that becomes `release/1`; `a` merges it into its own
/// change and lands the merge, and blame on `main` attributes the fix's
/// line to the release changeset.
async fn merge_a_release_fix(server: &str, repo: &str, a: &Path, b: &Path) {
    scn(b, &["update"]);
    let mut readme = std::fs::read(b.join("README")).unwrap();
    readme.extend_from_slice(b"fixed\n");
    std::fs::write(b.join("README"), &readme).unwrap();
    let fix = committed(&scn(b, &["commit", "-m", "fix"]));
    scion_client::Remote::connect(server)
        .await
        .unwrap()
        .create_ref(repo, "release/1", fix)
        .await
        .unwrap();

    std::fs::write(a.join("srv/api/merged.rs"), b"// a\n").unwrap();
    scn(a, &["add", "srv/api/merged.rs"]);
    scn(a, &["commit", "-m", "a before the merge"]);
    let out = scn(a, &["merge", "release/1"]);
    assert!(out.starts_with("merged release/1 into "), "{out}");
    assert_eq!(std::fs::read(a.join("README")).unwrap(), readme);
    assert!(scn(a, &["land"]).starts_with("landed "));

    let blame = scn(a, &["blame", "README", "-r", "main"]);
    assert!(
        blame.ends_with(&format!("{} fixed\n", &fix.to_hex()[..12])),
        "{blame}"
    );
}

/// Two clones commit non-overlapping edits and land at the same time,
/// with no `update` in between: the server rebases whichever comes second.
/// `a` is sparse, so `b`'s files at the root never reach it.
fn two_clones_land_at_once(a: &Path, b: &Path) {
    for round in 0..2 {
        std::fs::write(a.join(format!("srv/api/a{round}.rs")), b"// a\n").unwrap();
        scn(a, &["add", "srv/api"]);
        scn(a, &["commit", "-m", &format!("a {round}")]);
        std::fs::write(b.join(format!("b{round}.txt")), b"b\n").unwrap();
        scn(b, &["add", "."]);
        scn(b, &["commit", "-m", &format!("b {round}")]);

        let (la, lb) = std::thread::scope(|t| {
            let la = t.spawn(|| scn(a, &["land"]));
            let lb = scn(b, &["land"]);
            (la.join().unwrap(), lb)
        });
        assert!(
            la.starts_with("landed ") && lb.starts_with("landed "),
            "{la}{lb}"
        );
        assert!(!a.join(format!("b{round}.txt")).exists());
    }
    let log = scn(b, &["log", "-r", "main", "-n", "1000"]);
    assert!(!log.contains("Parents:"), "main stays linear: {log}");
    for msg in ["a 0", "b 0", "a 1", "b 1"] {
        assert!(log.contains(&format!("    {msg}\n")), "{msg} landed: {log}");
    }
}
