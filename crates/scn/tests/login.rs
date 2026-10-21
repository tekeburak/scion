//! `scn login` runs GitHub's device flow through the server, against a fake
//! GitHub, and keeps the token it gets for that server only. The web
//! interface signs a browser in with GitHub's web flow against the same fake.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, post};
use scion_server::{GitHub, TokenKey};
use scion_store::MemoryStore;
use tokio::net::TcpListener;

type Json = ([(header::HeaderName, &'static str); 1], String);

fn json(body: &str) -> Json {
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_owned(),
    )
}

/// A device code is its OAuth app's client id plus `-device`; the app
/// decides what happens: `ok` is pending once then authorized, `denied` is
/// denied, `expired` expires.
async fn device_code(Form(form): Form<HashMap<String, String>>) -> Json {
    json(&format!(
        r#"{{"device_code":"{}-device","user_code":"WDJB-MJHT","verification_uri":"https://github.com/login/device","expires_in":900,"interval":0}}"#,
        form["client_id"]
    ))
}

async fn access_token(
    State(polls): State<Arc<AtomicU32>>,
    Form(form): Form<HashMap<String, String>>,
) -> Json {
    // The web flow: app `ok` with secret `s3cret` trades code `web` for a token.
    if let Some(code) = form.get("code") {
        return match (
            form["client_id"].as_str(),
            form["client_secret"].as_str(),
            code.as_str(),
        ) {
            ("ok", "s3cret", "web") => json(r#"{"access_token":"gho_ok","token_type":"bearer"}"#),
            _ => json(
                r#"{"error":"bad_verification_code","error_description":"The code passed is incorrect or expired."}"#,
            ),
        };
    }
    assert_eq!(
        form["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    match form["device_code"].as_str() {
        "ok-device" if polls.fetch_add(1, Ordering::SeqCst) == 0 => {
            json(r#"{"error":"authorization_pending"}"#)
        }
        "ok-device" => json(r#"{"access_token":"gho_ok","token_type":"bearer","scope":""}"#),
        "denied-device" => json(
            r#"{"error":"access_denied","error_description":"The authorization request was denied."}"#,
        ),
        _ => json(r#"{"error":"expired_token"}"#),
    }
}

async fn user(headers: HeaderMap) -> Result<Json, StatusCode> {
    match headers.get(header::AUTHORIZATION) {
        Some(v) if v == "Bearer gho_ok" => Ok(json(r#"{"login":"octocat","id":1}"#)),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// The fake GitHub; returns its address, which serves both the web and the
/// API routes.
async fn fake_github() -> String {
    let app = axum::Router::new()
        .route("/login/device/code", post(device_code))
        .route("/login/oauth/access_token", post(access_token))
        .route("/user", get(user))
        .with_state(Arc::new(AtomicU32::new(0)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn key() -> TokenKey {
    TokenKey::from_hex(&"42".repeat(32)).unwrap()
}

/// A server on a fresh memory store logging in with GitHub app `client_id`
/// at `github`, or without GitHub login. Returns its address.
async fn serve(github: Option<(&str, &str)>) -> String {
    let store = Arc::new(MemoryStore::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let github = github.map(|(id, url)| GitHub::new(id, url, url));
    tokio::spawn(scion_server::serve(
        listener,
        store.clone(),
        store,
        key(),
        github,
    ));
    format!("http://{addr}")
}

/// Run `scn args` with `config` as its config directory; returns whether it
/// succeeded, stdout and stderr.
fn scn(config: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_scn"))
        .args(args)
        .env("XDG_CONFIG_HOME", config)
        .env_remove("SCN_TOKEN")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn token_file(config: &Path, server: &str) -> std::path::PathBuf {
    let name = scion_client::token_path(server).unwrap();
    config.join("scn/tokens").join(name.file_name().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_stores_a_token_of_the_github_user() {
    let github = fake_github().await;
    let server = serve(Some(("ok", &github))).await;
    let config = tempfile::tempdir().unwrap();
    let (ok, out, err) = scn(config.path(), &["login", &server]);
    assert!(ok, "scn login failed: {err}");
    assert!(
        out.starts_with("open https://github.com/login/device and enter the code WDJB-MJHT\n"),
        "{out}"
    );
    assert!(
        out.contains(&format!("logged in to {server} as github:octocat until ")),
        "{out}"
    );

    let path = token_file(config.path(), &server);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let token = std::fs::read_to_string(&path).unwrap();
    let (principal, _) = key().verify(token.trim(), SystemTime::now()).unwrap();
    assert_eq!(principal, "github:octocat");

    // Later commands send it: the server knows who is asking, and that it
    // may not create repositories.
    let (ok, _, err) = scn(config.path(), &["init", &server, "app"]);
    assert!(!ok);
    assert!(
        err.contains("github:octocat is not a server admin"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_denied_or_expired_code_says_which() {
    let github = fake_github().await;
    for (app, want) in [
        ("denied", "the login was denied on GitHub"),
        ("expired", "the GitHub code expired before it was entered"),
    ] {
        let server = serve(Some((app, &github))).await;
        let config = tempfile::tempdir().unwrap();
        let (ok, _, err) = scn(config.path(), &["login", &server]);
        assert!(!ok, "{app}: scn login succeeded");
        assert!(err.contains(want), "{app}: {err}");
        assert!(!token_file(config.path(), &server).exists(), "{app}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_without_github_login_says_so() {
    let server = serve(None).await;
    let config = tempfile::tempdir().unwrap();
    let (ok, _, err) = scn(config.path(), &["login", &server]);
    assert!(!ok);
    assert!(err.contains("this server has no GitHub login"), "{err}");
}

/// The web interface on a fresh memory store, signing in with app `ok` at
/// `github` with `secret`, or with no secret. Returns its address.
async fn serve_web(github: &str, secret: Option<&str>) -> String {
    let store = Arc::new(MemoryStore::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = GitHub::new("ok", github, github);
    let app = match secret {
        Some(s) => app.with_secret(s),
        None => app,
    };
    tokio::spawn(scion_server::serve_web(
        listener,
        store.clone(),
        store,
        key(),
        Some(app),
    ));
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_web_flow_signs_a_browser_in_as_the_github_user() {
    let github = fake_github().await;
    let web = serve_web(&github, Some("s3cret")).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let started = client.get(format!("{web}/login")).send().await.unwrap();
    assert_eq!(started.status().as_u16(), 303);
    let to = started.headers()["location"].to_str().unwrap();
    let state = to
        .strip_prefix(&format!(
            "{github}/login/oauth/authorize?client_id=ok&state="
        ))
        .expect(to)
        .to_owned();
    assert_eq!(state.len(), 64);
    let cookie = format!("scion_login={state}");

    let callback = async |code: &str| {
        client
            .get(format!("{web}/login/callback?code={code}&state={state}"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
    };
    let refused = callback("stale").await;
    assert_eq!(refused.status().as_u16(), 502);
    let page = refused.text().await.unwrap();
    assert!(
        page.contains("The code passed is incorrect or expired."),
        "{page}"
    );

    let back = callback("web").await;
    assert_eq!(back.status().as_u16(), 303);
    let session = back.headers()["set-cookie"].to_str().unwrap();
    let token = session
        .strip_prefix("scion_session=")
        .and_then(|rest| rest.split_once(';'))
        .expect(session)
        .0;
    assert!(session.contains("Max-Age=2592000;"), "{session}");
    let (principal, _) = key().verify(token, SystemTime::now()).unwrap();
    assert_eq!(principal, "github:octocat");

    let home = client
        .get(format!("{web}/"))
        .header("cookie", format!("scion_session={token}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        home.contains("Signed in as <strong>github:octocat</strong>"),
        "{home}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_client_secret_the_pages_have_no_sign_in() {
    let github = fake_github().await;
    let web = serve_web(&github, None).await;
    let response = reqwest::get(format!("{web}/login")).await.unwrap();
    assert_eq!(response.status().as_u16(), 404);
    let page = response.text().await.unwrap();
    assert!(page.contains("SCION_GITHUB_CLIENT_SECRET"), "{page}");
    let home = reqwest::get(format!("{web}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(home.contains("no GitHub sign-in"), "{home}");
}
