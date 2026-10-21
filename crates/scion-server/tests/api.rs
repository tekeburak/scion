//! The HTTP/JSON API on the memory stores: every route, and the init →
//! clone → commit → land loop with nothing but HTTP.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use scion_server::TokenKey;
use scion_store::{Grant, MemoryStore, MetadataStore, Role, SERVER_GRANTS};
use serde_json::{Value, json};
use tokio::net::TcpListener;

fn key() -> TokenKey {
    TokenKey::from_hex(&"5c".repeat(32)).unwrap()
}

fn token_of(principal: &str) -> String {
    key()
        .issue(principal, SystemTime::now() + Duration::from_secs(3600))
        .unwrap()
}

/// A server on fresh memory stores where `sp:test` may create repositories;
/// returns the API's base URL and the store.
async fn serve() -> (String, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let admin = Grant {
        principal: "sp:test".into(),
        path_prefix: String::new(),
        role: Role::Admin,
    };
    store.put_grant(SERVER_GRANTS, &admin).await.unwrap();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let web = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = web.local_addr().unwrap();
    tokio::spawn(scion_server::serve_with_web(
        grpc,
        web,
        store.clone(),
        store.clone(),
        key(),
        None,
    ));
    (format!("http://{addr}/api/v1"), store)
}

/// Calls the API as one principal, or as nobody.
struct Client {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Client {
    fn new(base: &str, principal: Option<&str>) -> Self {
        Self {
            base: base.to_owned(),
            token: principal.map(token_of),
            http: reqwest::Client::new(),
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> (u16, Value) {
        let request = match &self.token {
            Some(t) => request.bearer_auth(t),
            None => request,
        };
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let body = response.bytes().await.unwrap();
        let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, value)
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.send(self.http.get(format!("{}{path}", self.base)))
            .await
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.send(self.http.post(format!("{}{path}", self.base)).json(body))
            .await
    }

    async fn put(&self, path: &str, body: &Value) -> (u16, Value) {
        self.send(self.http.put(format!("{}{path}", self.base)).json(body))
            .await
    }

    async fn delete(&self, path: &str) -> (u16, Value) {
        self.send(self.http.delete(format!("{}{path}", self.base)))
            .await
    }

    /// GET `path` and insist on 200.
    async fn ok(&self, path: &str) -> Value {
        let (status, body) = self.get(path).await;
        assert_eq!(status, 200, "GET {path}: {body}");
        body
    }

    /// Store `content` as a blob of `repo`; its id.
    async fn blob(&self, repo: &str, content: &[u8]) -> String {
        let request = self
            .http
            .post(format!("{}/repos/{repo}/blobs", self.base))
            .body(content.to_vec());
        let (status, body) = self.send(request).await;
        assert_eq!(status, 201, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    /// Create a changeset of `repo` on `parent` writing `files`; its id.
    async fn commit(&self, repo: &str, parent: &str, files: &[(&str, &str)], msg: &str) -> String {
        let mut changes = serde_json::Map::new();
        for (path, content) in files {
            let id = self.blob(repo, content.as_bytes()).await;
            changes.insert(
                (*path).to_owned(),
                json!({"change": "tracked", "content": id, "kind": "regular", "size": content.len()}),
            );
        }
        let cs = json!({
            "parents": [parent],
            "author": {"name": "Ada", "email": "ada@example.com"},
            "author_date": {"unix_secs": 1_791_969_120, "tz_offset_minutes": 180},
            "message": msg,
            "file_changes": changes,
        });
        let (status, body) = self.post(&format!("/repos/{repo}/changesets"), &cs).await;
        assert_eq!(status, 201, "{body}");
        assert_eq!(body["root_tree"].as_str().unwrap().len(), 64);
        body["id"].as_str().unwrap().to_owned()
    }

    /// Land `head` on `repo`'s main; the finished request.
    async fn land(&self, repo: &str, head: &str) -> Value {
        let (status, cr) = self
            .post(
                &format!("/repos/{repo}/change-requests"),
                &json!({"head": head, "wait": true}),
            )
            .await;
        assert_eq!(status, 201, "{cr}");
        cr
    }
}

/// Every file under the tree `id`, path → content, read over HTTP.
async fn checkout(api: &Client, repo: &str, id: &str, prefix: &str) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut dirs = vec![(prefix.to_owned(), id.to_owned())];
    while let Some((dir, id)) = dirs.pop() {
        let tree = api.ok(&format!("/repos/{repo}/trees/{id}")).await;
        assert_eq!(tree["id"], id.as_str());
        for entry in tree["entries"].as_array().unwrap() {
            let name = format!("{dir}{}", entry["name"].as_str().unwrap());
            let id = entry["id"].as_str().unwrap();
            if entry["type"] == "tree" {
                dirs.push((format!("{name}/"), id.to_owned()));
                continue;
            }
            let mut request = api
                .http
                .get(format!("{}/repos/{repo}/blobs/{id}", api.base));
            if let Some(token) = &api.token {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status().as_u16(), 200);
            assert_eq!(
                response.headers()["content-type"],
                "application/octet-stream"
            );
            let bytes = response.bytes().await.unwrap();
            assert_eq!(entry["size"], bytes.len());
            files.push((name, String::from_utf8(bytes.to_vec()).unwrap()));
        }
    }
    files.sort();
    files
}

#[tokio::test]
async fn init_clone_commit_land_over_http_alone() {
    let (base, _) = serve().await;
    let api = Client::new(&base, Some("sp:test"));

    let (status, init) = api.post("/repos", &json!({"repo": "demo"})).await;
    assert_eq!(status, 201, "{init}");
    let root = init["target"].as_str().unwrap().to_owned();
    assert_eq!(init["version"], 0);
    let (status, again) = api.post("/repos", &json!({"repo": "demo"})).await;
    assert_eq!(
        (status, again["error"].as_str()),
        (409, Some("already_exists"))
    );

    // Clone: main, its root tree (empty), nothing to fetch.
    let main = api.ok("/repos/demo/refs/main").await;
    assert_eq!(main["target"], root.as_str());
    let tree = main["root_tree"].as_str().unwrap();
    assert_eq!(checkout(&api, "demo", tree, "").await, vec![]);

    // Commit and land; `main` fast-forwards to the changeset.
    let files = [
        ("README.md", "# Demo\n"),
        ("src/lib.rs", "pub fn one() {}\n"),
    ];
    let head = api.commit("demo", &root, &files, "Start the demo").await;
    let cr = api.land("demo", &head).await;
    assert_eq!(cr["status"], "landed", "{cr}");
    assert_eq!(cr["landed"], head.as_str());
    assert_eq!(cr["base"], root.as_str());

    // A second clone sees exactly what was committed.
    let main = api.ok("/repos/demo/refs/main").await;
    assert_eq!(main["target"], head.as_str());
    assert_eq!(main["version"], 1);
    let tree = main["root_tree"].as_str().unwrap();
    let want: Vec<(String, String)> = files
        .iter()
        .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
        .collect();
    assert_eq!(checkout(&api, "demo", tree, "").await, want);

    // A blob travels in one body, past the usual 2 MiB limit of a JSON API.
    let large = vec![b'x'; 3 << 20];
    let id = api.blob("demo", &large).await;
    assert_eq!(id, scion_types::BlobId::hash(&large).to_string());

    let cs = api.ok(&format!("/repos/demo/changesets/{head}")).await;
    assert_eq!(cs["id"], head.as_str());
    assert_eq!(cs["parents"], json!([root]));
    assert_eq!(cs["message"], "Start the demo");
    assert_eq!(cs["author"]["email"], "ada@example.com");
    assert_eq!(cs["author_date"]["tz_offset_minutes"], 180);
    assert_eq!(cs["file_changes"]["src/lib.rs"]["change"], "tracked");
    assert_eq!(cs["file_changes"]["src/lib.rs"]["kind"], "regular");
}

/// `demo` with `c1` (README.md, landed), `c2` on it (a.txt, landed) and
/// `c3` on `c1` (b.txt, not landed).
async fn history(api: &Client) -> (String, String, String) {
    let (_, init) = api.post("/repos", &json!({"repo": "demo"})).await;
    let root = init["target"].as_str().unwrap().to_owned();
    let c1 = api
        .commit("demo", &root, &[("README.md", "one\n")], "One")
        .await;
    api.land("demo", &c1).await;
    let c2 = api.commit("demo", &c1, &[("a.txt", "a\n")], "Add a").await;
    api.land("demo", &c2).await;
    let c3 = api.commit("demo", &c1, &[("b.txt", "b\n")], "Add b").await;
    (c1, c2, c3)
}

#[tokio::test]
async fn read_routes_mirror_the_control_plane() {
    let (base, _) = serve().await;
    let api = Client::new(&base, Some("sp:test"));
    let (c1, c2, c3) = history(&api).await;

    assert_eq!(api.ok("/repos").await, json!({"repos": ["demo"]}));

    let log = api.ok(&format!("/repos/demo/log?head={c2}")).await;
    let ids: Vec<&str> = log["changesets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 3, "{log}");
    assert_eq!(ids[..2], [c2.as_str(), c1.as_str()]);
    let log = api.ok(&format!("/repos/demo/log?head={c2}&limit=1")).await;
    assert_eq!(log["changesets"].as_array().unwrap().len(), 1);
    let log = api
        .ok(&format!("/repos/demo/log?head={c2}&path=a.txt"))
        .await;
    assert_eq!(log["changesets"][0]["id"], c2.as_str());
    assert_eq!(log["changesets"].as_array().unwrap().len(), 1);

    let diff = api.ok(&format!("/repos/demo/diff?old={c1}&new={c2}")).await;
    assert_eq!(diff["entries"][0]["path"], "a.txt");
    assert_eq!(diff["entries"][0]["old"], Value::Null);
    assert_eq!(diff["entries"][0]["new"]["size"], 2);
    let diff = api.ok(&format!("/repos/demo/diff?new={c1}")).await;
    assert_eq!(diff["entries"][0]["path"], "README.md");

    let base = api
        .ok(&format!("/repos/demo/merge-base?a={c2}&b={c3}"))
        .await;
    assert_eq!(base["base"], c1.as_str());

    let blame = api
        .ok(&format!("/repos/demo/blame?changeset={c2}&path=README.md"))
        .await;
    assert_eq!(blame["runs"], json!([{"changeset": c1, "lines": 1}]));

    let all = api.ok("/repos/demo/change-requests").await;
    let requests = all["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["head"], c2.as_str(), "newest first");
    let landed = api
        .ok("/repos/demo/change-requests?status=landed&limit=1")
        .await;
    assert_eq!(landed["requests"].as_array().unwrap().len(), 1);
    let none = api.ok("/repos/demo/change-requests?status=rejected").await;
    assert_eq!(none["requests"], json!([]));
    let id = requests[1]["id"].as_u64().unwrap();
    let one = api.ok(&format!("/repos/demo/change-requests/{id}")).await;
    assert_eq!(
        (&one["head"], &one["status"]),
        (&json!(c1), &json!("landed"))
    );
    let waited = api
        .ok(&format!("/repos/demo/change-requests/{id}?wait=true"))
        .await;
    assert_eq!(waited["status"], "landed");

    let landings = api
        .ok(&format!("/repos/demo/landings?changesets={c1},{c2},{c3}"))
        .await;
    let landings = landings["landings"].as_array().unwrap();
    assert_eq!(landings.len(), 2, "c3 never landed: {landings:?}");
    assert_eq!(landings[0]["changeset"], c1.as_str());
    assert_eq!(landings[0]["request"]["status"], "landed");

    let grants = api.ok("/repos/demo/grants").await;
    assert_eq!(
        grants["grants"],
        json!([{"principal": "sp:test", "path": "", "role": "admin"}])
    );

    let me = api.ok("/whoami?repo=demo").await;
    assert_eq!(me["principal"], "sp:test");
    assert_eq!(me["grants"][0]["role"], "admin");
    assert!(me["expires_unix_seconds"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn write_routes_rebase_merge_revert_move_refs_and_grant() {
    let (base, _) = serve().await;
    let api = Client::new(&base, Some("sp:test"));
    let (c1, c2, c3) = history(&api).await;

    let (status, rebased) = api
        .post("/repos/demo/rebase", &json!({"head": c3, "onto": c2}))
        .await;
    assert_eq!(status, 200, "{rebased}");
    assert_eq!(rebased["created"], 1);
    assert_eq!(rebased["conflicts"], json!([]));
    let on = rebased["changeset"].as_str().unwrap();
    let cs = api.ok(&format!("/repos/demo/changesets/{on}")).await;
    assert_eq!(cs["parents"], json!([c2]));

    let author = json!({"name": "Ada", "email": "ada@example.com"});
    let (status, merged) = api
        .post(
            "/repos/demo/merge",
            &json!({"ours": c2, "theirs": c3, "author": author, "message": "Merge b"}),
        )
        .await;
    assert_eq!(status, 200, "{merged}");
    assert_eq!(merged["created"], 1);
    let merge = merged["changeset"].as_str().unwrap();
    let cs = api.ok(&format!("/repos/demo/changesets/{merge}")).await;
    assert_eq!(cs["parents"], json!([c2, c3]));
    assert_eq!(cs["message"], "Merge b");

    let (status, reverted) = api
        .post(
            "/repos/demo/revert",
            &json!({
                "head": c2, "changeset": c2, "author": author, "message": "Revert a",
                "date": {"unix_secs": 1_800_000_000, "tz_offset_minutes": -300},
            }),
        )
        .await;
    assert_eq!(status, 200, "{reverted}");
    let undo = reverted["changeset"].as_str().unwrap();
    let cs = api.ok(&format!("/repos/demo/changesets/{undo}")).await;
    assert_eq!(cs["file_changes"]["a.txt"], json!({"change": "deleted"}));
    assert_eq!(cs["author_date"]["tz_offset_minutes"], -300);
    // c2 changed nothing that changed again after it, but a second revert
    // on top of the first finds a.txt changed since.
    let (_, again) = api
        .post(
            "/repos/demo/revert",
            &json!({"head": undo, "changeset": c2, "author": author, "message": "Again"}),
        )
        .await;
    assert_eq!(again["conflicts"], json!(["a.txt"]), "{again}");
    assert_eq!(again["changeset"], Value::Null);

    let (status, created) = api
        .put("/repos/demo/refs/release/1", &json!({"target": c1}))
        .await;
    assert_eq!((status, &created["version"]), (200, &json!(0)), "{created}");
    let (status, moved) = api
        .put(
            "/repos/demo/refs/release/1",
            &json!({"target": c2, "expected_version": 0}),
        )
        .await;
    assert_eq!((status, &moved["version"]), (200, &json!(1)), "{moved}");
    let (status, lost) = api
        .put(
            "/repos/demo/refs/release/1",
            &json!({"target": c2, "expected_version": 0}),
        )
        .await;
    assert_eq!(
        (status, lost["error"].as_str()),
        (409, Some("failed_precondition"))
    );
    let release = api.ok("/repos/demo/refs/release/1").await;
    assert_eq!(release["target"], c2.as_str());

    let grant = json!({"principal": "sp:dev", "role": "writer", "path": "docs"});
    let (status, _) = api.put("/repos/demo/grants", &grant).await;
    assert_eq!(status, 204);
    let grants = api.ok("/repos/demo/grants").await;
    assert!(
        grants["grants"].as_array().unwrap().contains(&grant),
        "{grants}"
    );
    let (status, _) = api
        .delete("/repos/demo/grants?principal=sp:dev&path=docs")
        .await;
    assert_eq!(status, 204);
    let (status, body) = api
        .delete("/repos/demo/grants?principal=sp:dev&path=docs")
        .await;
    assert_eq!((status, body["error"].as_str()), (404, Some("not_found")));
}

#[tokio::test]
async fn public_repositories_read_without_a_token_and_nothing_writes_without_one() {
    let (base, _) = serve().await;
    let admin = Client::new(&base, Some("sp:test"));
    let (c1, c2, _) = history(&admin).await;
    let anonymous = Client::new(&base, None);
    let nobody = Client::new(&base, Some("sp:nobody"));

    let (status, body) = anonymous.get("/repos/demo/refs/main").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (401, Some("unauthenticated")),
        "{body}"
    );
    assert!(
        body["message"].as_str().unwrap().contains("not public"),
        "{body}"
    );
    let (status, body) = nobody.get("/repos/demo/refs/main").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (403, Some("permission_denied"))
    );
    assert_eq!(anonymous.ok("/repos").await, json!({"repos": []}));

    let public = json!({"principal": "anyone", "role": "reader"});
    let (status, _) = admin.put("/repos/demo/grants", &public).await;
    assert_eq!(status, 204);
    for client in [&anonymous, &nobody] {
        let main = client.ok("/repos/demo/refs/main").await;
        assert_eq!(main["target"], c2.as_str());
        client.ok(&format!("/repos/demo/log?head={c2}")).await;
        client.ok("/repos/demo/change-requests").await;
        assert_eq!(client.ok("/repos").await, json!({"repos": ["demo"]}));
        let tree = main["root_tree"].as_str().unwrap();
        assert_eq!(checkout(client, "demo", tree, "").await.len(), 2);
    }

    // Reads only: writes need a token, and a token needs a role.
    let (status, body) = anonymous
        .post("/repos/demo/change-requests", &json!({"head": c1}))
        .await;
    assert_eq!(status, 401, "{body}");
    let (status, body) = nobody
        .post("/repos/demo/change-requests", &json!({"head": c1}))
        .await;
    assert_eq!(status, 403, "{body}");
    let (status, _) = anonymous.get("/whoami").await;
    assert_eq!(status, 401);
    let (status, body) = admin
        .put(
            "/repos/demo/grants",
            &json!({"principal": "anyone", "role": "writer"}),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("anyone can only be a reader")
    );
}

#[tokio::test]
async fn bad_requests_are_refused_by_name() {
    let (base, _) = serve().await;
    let api = Client::new(&base, Some("sp:test"));
    let (c1, _, _) = history(&api).await;
    for (path, status, says) in [
        ("/repos/demo/changesets/zz".to_owned(), 400, "not an id"),
        ("/repos/demo/log?head=12".to_owned(), 400, "head"),
        ("/repos/demo/refs/topic".to_owned(), 404, "topic"),
        (
            format!("/repos/demo/blame?changeset={c1}&path=..%2Fx"),
            400,
            "path",
        ),
        (
            format!("/repos/demo/blame?changeset={c1}&path=nope"),
            404,
            "nope",
        ),
        (
            "/repos/demo/change-requests?status=done".to_owned(),
            400,
            "status",
        ),
        ("/repos/demo/change-requests/99".to_owned(), 404, "99"),
        (format!("/repos/demo/trees/{c1}"), 404, "tree"),
    ] {
        let (got, body) = api.get(&path).await;
        assert_eq!(got, status, "{path}: {body}");
        assert!(
            body["message"].as_str().unwrap().contains(says),
            "{path}: {body}"
        );
    }
    let bad = json!({
        "parents": [c1],
        "author": {"name": "Ada", "email": "a@example.com"},
        "author_date": {"unix_secs": 0, "tz_offset_minutes": 0},
        "message": "x",
        "file_changes": {"/abs": {"change": "deleted"}},
    });
    let (status, body) = api.post("/repos/demo/changesets", &bad).await;
    assert_eq!(status, 400, "{body}");
    let forged = reqwest::Client::new()
        .get(format!("{base}/repos/demo/refs/main"))
        .bearer_auth("forged")
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status().as_u16(), 401);
}
