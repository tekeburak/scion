//! Every request `scn` makes carries the token of the working copy's server.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use scion_store::MemoryStore;
use tokio::net::TcpListener;
use tonic::body::Body;
use tonic::codegen::http::Request;
use tonic::transport::server::TcpIncoming;

/// `(server, gRPC method, authorization header)` of every request seen.
type Seen = Arc<Mutex<Vec<(&'static str, String, Option<String>)>>>;

/// A server on a fresh memory store that records every request into `seen`
/// under `name`. Returns its address.
async fn serve(name: &'static str, seen: Seen) -> String {
    let store = Arc::new(MemoryStore::new());
    let routes = scion_server::routes(store.clone(), store).await.unwrap();
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

/// Run `scn args` in `dir` with `config` as its config directory; panics
/// unless it succeeds.
fn scn(dir: &Path, config: &Path, env_token: Option<&str>, args: &[&str]) -> String {
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
    assert!(
        out.status.success(),
        "scn {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_rpc_carries_the_token_of_the_working_copy_server() {
    let seen = Seen::default();
    let a = serve("a", seen.clone()).await;
    let b = serve("b", seen.clone()).await;
    let config = tempfile::tempdir().unwrap();
    for (server, token) in [(&a, "aaaa"), (&b, "bbbb")] {
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
    scn(&wc, cfg, Some("cccc"), &["log"]);

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
            "ListChangeRequests",
            "Log",
            "Merge",
            "MoveRef",
            "PathLog",
            "PutBlob",
            "Rebase",
            "Revert",
            "SubmitChangeRequest",
        ])
    );
    for (i, (server, method, auth)) in seen.iter().enumerate() {
        let want = match *server {
            _ if i >= before => "Bearer cccc",
            "a" => "Bearer aaaa",
            _ => "Bearer bbbb",
        };
        assert_eq!(auth.as_deref(), Some(want), "{server} {method}");
    }
}
