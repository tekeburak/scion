//! The MCP tools against a server on the memory stores: what an inspector
//! lists, and a commit → land loop run through the tools alone, over stdio's
//! byte stream and over streamable HTTP.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rmcp::model::{CallToolRequestParams, CallToolResult, ReadResourceRequestParams};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use scion_mcp::Scion;
use scion_server::TokenKey;
use scion_store::{Grant, MemoryStore, MetadataStore, Role, SERVER_GRANTS};
use serde_json::{Value, json};
use tokio::net::TcpListener;

fn key() -> TokenKey {
    TokenKey::from_hex(&"5c".repeat(32)).unwrap()
}

fn token() -> String {
    key()
        .issue("sp:agent", SystemTime::now() + Duration::from_secs(3600))
        .unwrap()
}

/// A server where `sp:agent` may create repositories, with repository
/// `demo` created; the API's base URL.
async fn serve() -> String {
    let store = Arc::new(MemoryStore::new());
    let admin = Grant {
        principal: "sp:agent".into(),
        path_prefix: String::new(),
        role: Role::Admin,
    };
    store.put_grant(SERVER_GRANTS, &admin).await.unwrap();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let web = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1", web.local_addr().unwrap());
    tokio::spawn(scion_server::serve_with_web(
        grpc,
        web,
        store.clone(),
        store,
        key(),
        None,
    ));
    let created = reqwest::Client::new()
        .post(format!("{base}/repos"))
        .bearer_auth(token())
        .json(&json!({"repo": "demo"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    base
}

/// A client connected to `scion` through an in-memory byte stream, as
/// stdio connects one.
async fn connect(scion: Scion) -> RunningService<RoleClient, ()> {
    let (server, client) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move { scion.serve(server).await.unwrap().waiting().await });
    ().serve(client).await.unwrap()
}

async fn call_raw(
    client: &RunningService<RoleClient, ()>,
    tool: &str,
    args: Value,
) -> CallToolResult {
    let args = args.as_object().unwrap().clone();
    let params = CallToolRequestParams::new(tool.to_owned()).with_arguments(args);
    client.call_tool(params).await.unwrap()
}

fn text_of(result: &CallToolResult) -> String {
    let content = result.content.first().and_then(|c| c.as_text());
    content.map(|t| t.text.clone()).unwrap_or_default()
}

/// Call `tool` and insist it succeeded; its text.
async fn call(client: &RunningService<RoleClient, ()>, tool: &str, args: Value) -> String {
    let result = call_raw(client, tool, args).await;
    assert_ne!(result.is_error, Some(true), "{tool}: {}", text_of(&result));
    text_of(&result)
}

async fn call_json(client: &RunningService<RoleClient, ()>, tool: &str, args: Value) -> Value {
    serde_json::from_str(&call(client, tool, args).await).unwrap()
}

/// Commit `files` on main and land them; the landed changeset.
async fn commit_and_land(
    client: &RunningService<RoleClient, ()>,
    files: Value,
    msg: &str,
) -> String {
    let commit = json!({
        "repo": "demo", "message": msg, "author_name": "Agent",
        "author_email": "agent@example.com", "files": files,
    });
    let committed = call_json(client, "write_files_and_commit", commit).await;
    let head = committed["changeset"].as_str().unwrap().to_owned();
    let landed = call_json(client, "land", json!({"repo": "demo", "head": head})).await;
    assert_eq!(landed["status"], "landed", "{landed}");
    landed["landed"].as_str().unwrap().to_owned()
}

const TOOLS: [&str; 10] = [
    "blame",
    "change_request",
    "changeset",
    "diff",
    "land",
    "list_repos",
    "log",
    "read_file",
    "tree",
    "write_files_and_commit",
];

#[tokio::test]
async fn an_inspector_lists_every_tool_and_the_file_resource() {
    let client = connect(Scion::new(&serve().await, Some(token()))).await;
    let info = client.peer_info().unwrap();
    assert_eq!(info.server_info.as_ref().unwrap().name, "scion-mcp");
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(names, TOOLS);
    for tool in &tools {
        assert!(tool.description.as_deref().is_some_and(|d| !d.is_empty()));
        let writes = ["land", "write_files_and_commit"].contains(&&*tool.name);
        let read_only = tool.annotations.as_ref().and_then(|a| a.read_only_hint);
        assert_eq!(read_only == Some(true), !writes, "{}", tool.name);
    }
    let commit = tools
        .iter()
        .find(|t| t.name == "write_files_and_commit")
        .unwrap();
    let required = &commit.input_schema["required"];
    for field in ["repo", "message", "author_name", "author_email", "files"] {
        assert!(
            required.as_array().unwrap().contains(&json!(field)),
            "{field}"
        );
    }
    let templates = client.list_all_resource_templates().await.unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(
        templates[0].uri_template,
        "scion://{repo}/{+path}@{revision}"
    );
}

#[tokio::test]
async fn commit_then_land_through_the_tools_alone() {
    let client = connect(Scion::new(&serve().await, Some(token()))).await;
    let repos = call_json(&client, "list_repos", json!({})).await;
    assert_eq!(repos["repos"], json!(["demo"]));

    let files = json!([
        {"path": "src/lib.rs", "content": "pub fn one() -> u32 { 1 }\n"},
        {"path": "run.sh", "content": "#!/bin/sh\n", "executable": true},
    ]);
    let first = commit_and_land(&client, files, "Start").await;
    let read = json!({"repo": "demo", "path": "src/lib.rs"});
    assert_eq!(
        call(&client, "read_file", read).await,
        "pub fn one() -> u32 { 1 }\n"
    );

    let root = call_json(&client, "tree", json!({"repo": "demo"})).await;
    assert_eq!(root["changeset"], first.as_str());
    let entries = root["entries"].as_array().unwrap();
    let names: Vec<_> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["run.sh", "src"]);
    assert_eq!(entries[0]["kind"], "executable");

    // A second change edits one file and deletes the other.
    let files = json!([
        {"path": "src/lib.rs", "content": "pub fn one() -> u32 { 1 }\npub fn two() -> u32 { 2 }\n"},
        {"path": "run.sh"},
    ]);
    let second = commit_and_land(&client, files, "Add two, drop run.sh").await;

    let log = call_json(&client, "log", json!({"repo": "demo", "limit": 2})).await;
    let messages: Vec<_> = log["changesets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["message"].as_str().unwrap())
        .collect();
    assert_eq!(messages, ["Add two, drop run.sh", "Start"]);
    let by_path = json!({"repo": "demo", "path": "src"});
    let log = call_json(&client, "log", by_path).await;
    assert_eq!(log["changesets"].as_array().unwrap().len(), 2);

    // The diff defaults to the first parent.
    let diff = call_json(&client, "diff", json!({"repo": "demo", "new": "main"})).await;
    let paths: Vec<_> = diff["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["run.sh", "src/lib.rs"]);
    assert!(diff["entries"][0]["new"].is_null());

    let blame = json!({"repo": "demo", "path": "src/lib.rs"});
    let blame = call_json(&client, "blame", blame).await;
    assert_eq!(
        blame["runs"],
        json!([{"changeset": first, "lines": 1}, {"changeset": second, "lines": 1}])
    );

    let cs = json!({"repo": "demo", "id": second});
    let cs = call_json(&client, "changeset", cs).await;
    assert_eq!(cs["parents"], json!([first]));
    assert_eq!(cs["file_changes"]["run.sh"]["change"], "deleted");

    // The old revision still reads; the deleted file is gone at main.
    let at_first = json!({"repo": "demo", "path": "run.sh", "at": first});
    assert_eq!(call(&client, "read_file", at_first).await, "#!/bin/sh\n");
    let gone = call_raw(
        &client,
        "read_file",
        json!({"repo": "demo", "path": "run.sh"}),
    )
    .await;
    assert_eq!(gone.is_error, Some(true));
    assert!(
        text_of(&gone).contains("no such file"),
        "{}",
        text_of(&gone)
    );

    let requests = call_json(&client, "change_request", json!({"repo": "demo"})).await;
    assert_eq!(requests["requests"].as_array().unwrap().len(), 2);
    let landed = json!({"repo": "demo", "status": "landed"});
    let landed = call_json(&client, "change_request", landed).await;
    let id = landed["requests"][0]["id"].as_u64().unwrap();
    let one = call_json(&client, "change_request", json!({"repo": "demo", "id": id})).await;
    assert_eq!(one["landed"], second.as_str());

    // Resources name a path at a revision.
    let read = |uri: String| client.read_resource(ReadResourceRequestParams::new(uri));
    let file = read("scion://demo/src/lib.rs@main".into()).await.unwrap();
    let text = serde_json::to_value(&file.contents[0]).unwrap();
    assert!(text["text"].as_str().unwrap().contains("two"));
    let old = read(format!("scion://demo/run.sh@{first}")).await.unwrap();
    assert_eq!(
        serde_json::to_value(&old.contents[0]).unwrap()["text"],
        "#!/bin/sh\n"
    );
    let dir = read("scion://demo/@main".into()).await.unwrap();
    let listing = serde_json::to_value(&dir.contents[0]).unwrap();
    assert!(listing["text"].as_str().unwrap().contains("\"src\""));
    assert!(read("scion://demo/missing@main".into()).await.is_err());
}

#[tokio::test]
async fn without_a_token_nothing_reads_or_writes_a_private_repository() {
    let client = connect(Scion::new(&serve().await, None)).await;
    let repos = call_json(&client, "list_repos", json!({})).await;
    assert_eq!(repos["repos"], json!([]));
    let read = call_raw(&client, "tree", json!({"repo": "demo"})).await;
    assert_eq!(read.is_error, Some(true));
    assert!(
        text_of(&read).starts_with("unauthenticated:"),
        "{}",
        text_of(&read)
    );
    let commit = json!({
        "repo": "demo", "parent": "0".repeat(64), "message": "x", "author_name": "A",
        "author_email": "a@example.com", "files": [{"path": "a", "content": "a"}],
    });
    let write = call_raw(&client, "write_files_and_commit", commit).await;
    assert_eq!(write.is_error, Some(true));
}

#[tokio::test]
async fn streamable_http_serves_the_same_tools() {
    let base = serve().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(Scion::new(&base, Some(token())).serve_http(listener));
    let client = ().serve(StreamableHttpClientTransport::from_uri(url)).await.unwrap();
    assert_eq!(client.list_all_tools().await.unwrap().len(), TOOLS.len());
    commit_and_land(
        &client,
        json!([{"path": "a.txt", "content": "a\n"}]),
        "Over HTTP",
    )
    .await;
    let read = json!({"repo": "demo", "path": "a.txt"});
    assert_eq!(call(&client, "read_file", read).await, "a\n");
}
