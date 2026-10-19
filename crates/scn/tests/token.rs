//! Every request `scn` makes carries the token of the working copy's server.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use scion_server::TokenKey;
use scion_store::{Grant, MemoryStore, MetadataStore, Role, SERVER_GRANTS};
use tokio::net::TcpListener;
use tonic::body::Body;
use tonic::codegen::http::Request;
use tonic::transport::server::TcpIncoming;

/// `(server, gRPC method, authorization header)` of every request seen.
type Seen = Arc<Mutex<Vec<(&'static str, String, Option<String>)>>>;

/// The key of server `name`: each server signs with its own.
fn key(name: &str) -> TokenKey {
    TokenKey::from_hex(&format!("{:02x}", name.as_bytes()[0]).repeat(32)).unwrap()
}

/// A token of server `name`'s key for `principal`, valid for an hour.
fn token(name: &str, principal: &str) -> String {
    let hour = Duration::from_secs(3600);
    scion_server::token(&key(name), principal, hour, SystemTime::now()).unwrap()
}

/// A server on a fresh memory store that records every request into `seen`
/// under `name`. Returns its address.
async fn serve(name: &'static str, seen: Seen) -> String {
    let store = Arc::new(MemoryStore::new());
    // `sp:a` and `sp:b` may create repositories.
    for principal in ["sp:a", "sp:b"] {
        let admin = Grant {
            principal: principal.into(),
            path_prefix: String::new(),
            role: Role::Admin,
        };
        store.put_grant(SERVER_GRANTS, &admin).await.unwrap();
    }
    let routes = scion_server::routes(store.clone(), store, key(name))
        .await
        .unwrap();
    let record = tower::ServiceBuilder::new().map_request(move |req: Request<Body>| {
        let auth = req
            .headers()
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_owned());
        let method = req.uri().path().to_owned();
        seen.lock().unwrap().push((name, method, auth));
        req
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .layer(record)
            .add_routes(routes)
            .serve_with_incoming(TcpIncoming::from(listener)),
    );
    format!("http://{addr}")
}

/// Run `scn args` in `dir` with `config` as its config directory; returns
/// whether it succeeded, stdout and stderr.
fn scn_try(
    dir: &Path,
    config: &Path,
    env_token: Option<&str>,
    args: &[&str],
) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scn"));
    cmd.current_dir(dir)
        .args(args)
        .env("XDG_CONFIG_HOME", config)
        .env("SCN_AUTHOR", "Ada <ada@example.com>")
        .env_remove("SCN_TOKEN");
    if let Some(t) = env_token {
        cmd.env("SCN_TOKEN", t);
    }
    let out = cmd.output().unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// [`scn_try`] that panics unless `scn` succeeds. Returns stdout.
fn scn(dir: &Path, config: &Path, env_token: Option<&str>, args: &[&str]) -> String {
    let (ok, out, err) = scn_try(dir, config, env_token, args);
    assert!(ok, "scn {args:?} failed: {err}");
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_rpc_carries_the_token_of_the_working_copy_server() {
    let seen = Seen::default();
    let a = serve("a", seen.clone()).await;
    let b = serve("b", seen.clone()).await;
    let config = tempfile::tempdir().unwrap();
    let (ta, tb) = (token("a", "sp:a"), token("b", "sp:b"));
    // Another token of `sp:a`, later expiry.
    let tc = key("a")
        .issue("sp:a", SystemTime::now() + Duration::from_secs(7200))
        .unwrap();
    for (server, token) in [(&a, &ta), (&b, &tb)] {
        // The file name `scn` derives from the server address, under the
        // config directory it is given.
        let name = scion_client::token_path(server).unwrap();
        let path = config
            .path()
            .join("scn/tokens")
            .join(name.file_name().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{token}\n")).unwrap();
    }
    let cfg = config.path();
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    let wc = top.join("wa");
    let scn_a = |args: &[&str]| scn(&wc, cfg, None, args);

    scn(top, cfg, None, &["init", &a, "demo"]);
    scn(top, cfg, None, &["clone", &a, "demo", "wa"]);
    std::fs::write(wc.join("f.txt"), "one\n").unwrap();
    scn_a(&["add", "f.txt"]);
    scn_a(&["commit", "-m", "one"]);
    scn_a(&["land"]);
    scn_a(&["land", "--status", "1"]);
    scn_a(&["requests"]);
    scn_a(&["log", "--landed"]);
    scn_a(&["log", "f.txt"]);
    scn_a(&["blame", "f.txt"]);
    scn_a(&["diff", "main", "@"]);
    std::fs::write(wc.join("f.txt"), "one\ntwo\n").unwrap();
    scn_a(&["diff"]);
    scn_a(&["commit", "-m", "two"]);
    scn_a(&["update"]);
    scn_a(&["merge", "main"]);
    scn_a(&["revert", "@"]);
    scn_a(&["checkout", "main"]);

    scn(top, cfg, None, &["init", &b, "demo"]);
    scn(top, cfg, None, &["clone", &b, "demo", "wb"]);
    scn(&top.join("wb"), cfg, None, &["log"]);
    // SCN_TOKEN wins over the per-server file.
    let before = seen.lock().unwrap().len();
    scn(&wc, cfg, Some(&tc), &["log"]);

    let seen = seen.lock().unwrap().clone();
    assert!(seen.len() > before);
    let methods: BTreeSet<&str> = seen
        .iter()
        .filter(|(s, ..)| *s == "a")
        .map(|(_, m, _)| m.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(
        methods,
        BTreeSet::from([
            "Blame",
            "CreateChangeset",
            "Diff",
            "FindLanding",
            "GetBlob",
            "GetChangeRequest",
            "GetRef",
            "GetTree",
            "InitRepo",
            "ListChangeRequests",
            "Log",
            "Merge",
            "PathLog",
            "PutBlob",
            "Rebase",
            "Revert",
            "SubmitChangeRequest",
        ])
    );
    for (i, (server, method, auth)) in seen.iter().enumerate() {
        let want = match *server {
            _ if i >= before => &tc,
            "a" => &ta,
            _ => &tb,
        };
        assert_eq!(
            auth.as_deref(),
            Some(format!("Bearer {want}").as_str()),
            "{server} {method}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_without_a_valid_token_names_scn_login() {
    let a = serve("a", Seen::default()).await;
    let config = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    for token in [None, Some(token("b", "sp:a").as_str())] {
        let (ok, _, err) = scn_try(dir.path(), config.path(), token, &["init", &a, "demo"]);
        assert!(!ok);
        assert!(err.contains("scn login"), "{err}");
    }
}
