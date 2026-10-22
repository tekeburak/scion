//! Write limits at every surface: every write route of the gRPC services,
//! the HTTP API and the MCP tools refuses a request without a token, and
//! every route a blob enters by refuses one over the per-blob limit, naming
//! it.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use scion_mcp::Scion;
use scion_proto::v1 as pb;
use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::object_service_client::ObjectServiceClient;
use scion_server::{MAX_MESSAGE_BYTES, TokenKey};
use scion_store::{Grant, MemoryStore, MetadataStore, Role, SERVER_GRANTS};
use scion_types::MAX_BLOB_BYTES;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request};

/// What every refusal of an oversized blob says.
const NAMED: &str = "per-blob limit of 67108864 bytes (64 MiB)";

fn key() -> TokenKey {
    TokenKey::from_hex(&"5c".repeat(32)).unwrap()
}

fn token() -> String {
    key()
        .issue("sp:agent", SystemTime::now() + Duration::from_secs(3600))
        .unwrap()
}

/// A server where `sp:agent` administers everything and repository `demo`
/// exists: its gRPC address and its API's base URL.
async fn serve() -> (String, String) {
    let store = Arc::new(MemoryStore::new());
    let admin = Grant {
        principal: "sp:agent".into(),
        path_prefix: String::new(),
        role: Role::Admin,
    };
    store.put_grant(SERVER_GRANTS, &admin).await.unwrap();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let web = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_url = format!("http://{}", grpc.local_addr().unwrap());
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
    (grpc_url, base)
}

/// `message` as a gRPC request, with `token` if given.
fn request<T>(message: T, token: Option<&str>) -> Request<T> {
    let mut request = Request::new(message);
    if let Some(t) = token {
        let value: MetadataValue<_> = format!("Bearer {t}").parse().unwrap();
        request.metadata_mut().insert("authorization", value);
    }
    request
}

/// Every gRPC write route, called without a token: the code each answers.
#[allow(clippy::too_many_lines)] // one row per RPC
async fn grpc_anonymous(channel: Channel) -> Vec<(&'static str, Code)> {
    let mut c = ControlServiceClient::new(channel.clone());
    let mut o = ObjectServiceClient::new(channel);
    let repo = || "demo".to_owned();
    let code = |r: Result<(), tonic::Status>| r.err().map_or(Code::Ok, |s| s.code());
    vec![
        (
            "PutBlob",
            code(
                o.put_blob(request(
                    pb::PutBlobRequest {
                        repo: repo(),
                        data: vec![1],
                    },
                    None,
                ))
                .await
                .map(drop),
            ),
        ),
        (
            "PutTree",
            code(
                o.put_tree(request(pb::PutTreeRequest::default(), None))
                    .await
                    .map(drop),
            ),
        ),
        (
            "InitRepo",
            code(
                c.init_repo(request(pb::InitRepoRequest { repo: "new".into() }, None))
                    .await
                    .map(drop),
            ),
        ),
        (
            "CreateChangeset",
            code(
                c.create_changeset(request(pb::CreateChangesetRequest::default(), None))
                    .await
                    .map(drop),
            ),
        ),
        (
            "MoveRef",
            code(
                c.move_ref(request(pb::MoveRefRequest::default(), None))
                    .await
                    .map(drop),
            ),
        ),
        (
            "Rebase",
            code(
                c.rebase(request(pb::RebaseRequest::default(), None))
                    .await
                    .map(drop),
            ),
        ),
        (
            "Merge",
            code(
                c.merge(request(pb::MergeRequest::default(), None))
                    .await
                    .map(drop),
            ),
        ),
        (
            "Revert",
            code(
                c.revert(request(pb::RevertRequest::default(), None))
                    .await
                    .map(drop),
            ),
        ),
        (
            "SubmitChangeRequest",
            code(
                c.submit_change_request(request(pb::SubmitChangeRequestRequest::default(), None))
                    .await
                    .map(drop),
            ),
        ),
        (
            "Grant",
            code(
                c.grant(request(pb::GrantRequest::default(), None))
                    .await
                    .map(drop),
            ),
        ),
        (
            "Revoke",
            code(
                c.revoke(request(pb::RevokeRequest::default(), None))
                    .await
                    .map(drop),
            ),
        ),
    ]
}

/// Every HTTP API write route: method and path.
const HTTP_WRITES: [(&str, &str); 10] = [
    ("POST", "/repos"),
    ("PUT", "/repos/demo/refs/main"),
    ("POST", "/repos/demo/blobs"),
    ("POST", "/repos/demo/changesets"),
    ("POST", "/repos/demo/rebase"),
    ("POST", "/repos/demo/merge"),
    ("POST", "/repos/demo/revert"),
    ("POST", "/repos/demo/change-requests"),
    ("PUT", "/repos/demo/grants"),
    ("DELETE", "/repos/demo/grants"),
];

/// A client of `scion` over an in-memory byte stream, served as stdio is.
async fn connect(scion: Scion) -> RunningService<RoleClient, ()> {
    let (server, client) = tokio::io::duplex(1 << 16);
    let (read, write) = tokio::io::split(server);
    tokio::spawn(scion.serve_stream(read, write));
    ().serve(client).await.unwrap()
}

async fn call(client: &RunningService<RoleClient, ()>, tool: &str, args: Value) -> CallToolResult {
    let args = args.as_object().unwrap().clone();
    let params = CallToolRequestParams::new(tool.to_owned()).with_arguments(args);
    client.call_tool(params).await.unwrap()
}

fn text_of(result: &CallToolResult) -> String {
    let content = result.content.first().and_then(|c| c.as_text());
    content.map(|t| t.text.clone()).unwrap_or_default()
}

fn commit(content: &str) -> Value {
    json!({
        "repo": "demo", "message": "m", "author_name": "Agent",
        "author_email": "agent@example.com",
        "files": [{"path": "big.bin", "content": content}],
    })
}

#[tokio::test]
async fn every_write_route_refuses_anonymous_writes() {
    let (grpc, base) = serve().await;

    let channel = Channel::from_shared(grpc).unwrap().connect().await.unwrap();
    for (rpc, code) in grpc_anonymous(channel).await {
        assert_eq!(code, Code::Unauthenticated, "gRPC {rpc}");
    }

    let http = reqwest::Client::new();
    for (method, path) in HTTP_WRITES {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let reply = http
            .request(method.clone(), format!("{base}{path}"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 401, "{method} {path}");
        let body: Value = reply.json().await.unwrap();
        assert_eq!(body["error"], "unauthenticated", "{method} {path}: {body}");
    }

    // The MCP server with no token: its write tools pass the API's refusal on.
    let client = connect(Scion::new(&base, None)).await;
    let head = "0".repeat(64);
    let writes = [
        ("write_files_and_commit", commit("hello")),
        ("land", json!({"repo": "demo", "head": head})),
    ];
    for (tool, args) in writes {
        let result = call(&client, tool, args).await;
        assert_eq!(result.is_error, Some(true), "{tool}");
        let text = text_of(&result);
        assert!(text.starts_with("unauthenticated"), "{tool}: {text}");
    }
}

#[tokio::test]
async fn every_blob_route_refuses_a_blob_over_the_limit_by_name() {
    let (grpc, base) = serve().await;
    let over = MAX_BLOB_BYTES + 1;

    let channel = Channel::from_shared(grpc).unwrap().connect().await.unwrap();
    let mut objects = ObjectServiceClient::new(channel)
        .max_encoding_message_size(MAX_MESSAGE_BYTES)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    let blob = pb::PutBlobRequest {
        repo: "demo".into(),
        data: vec![b'a'; over],
    };
    let refused = objects
        .put_blob(request(blob, Some(&token())))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::ResourceExhausted);
    assert!(
        refused.message().contains(NAMED),
        "gRPC: {}",
        refused.message()
    );

    // Just over the limit reaches the check; far over it is refused by the
    // body limit, under the same name.
    let http = reqwest::Client::new();
    for size in [over, MAX_MESSAGE_BYTES + 1] {
        let reply = http
            .post(format!("{base}/repos/demo/blobs"))
            .bearer_auth(token())
            .body(vec![b'a'; size])
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 413, "HTTP {size} bytes");
        let body: Value = reply.json().await.unwrap();
        assert_eq!(
            body["error"], "resource_exhausted",
            "HTTP {size} bytes: {body}"
        );
        let message = body["message"].as_str().unwrap();
        assert!(message.contains(NAMED), "HTTP {size} bytes: {message}");
    }

    let client = connect(Scion::new(&base, Some(token()))).await;
    let result = call(&client, "write_files_and_commit", commit(&"a".repeat(over))).await;
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    assert!(text.contains(NAMED), "MCP: {text}");

    // At the limit itself every surface takes the blob.
    let at = pb::PutBlobRequest {
        repo: "demo".into(),
        data: vec![b'a'; MAX_BLOB_BYTES],
    };
    objects.put_blob(request(at, Some(&token()))).await.unwrap();
}
