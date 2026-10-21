//! `scion-mcp`: Scion for agents, as a Model Context Protocol server.
//!
//! The tools read and write a server's repositories through its HTTP/JSON
//! API (`docs/API.md`), with one bearer token for every call, so roles,
//! directory grants and the land worker are the server's own; this crate only
//! names the operations an agent needs and walks trees for it. A revision
//! (`at`, `head`, `old`, `new`, `parent`) is a server ref (`main`,
//! `release/*`) or a 64-hex changeset id.
//!
//! Resources are files and directories at a revision:
//! `scion://<repo>/<path>@<revision>`.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    Implementation, ListResourceTemplatesResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, ResourceContents, ResourceTemplate,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// Every API call gets at most this long, the server's own per-request
/// deadline; a land that waits answers within it.
const DEADLINE: Duration = Duration::from_secs(30);

/// What a tool answers with when it fails: the server's refusal, as text the
/// agent reads.
type Answer = Result<String, String>;

/// The MCP server: tools and resources over one server's API.
#[derive(Debug, Clone)]
pub struct Scion {
    api: Api,
}

#[derive(Debug, Clone)]
struct Api {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Api {
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
        let request = match &self.token {
            Some(t) => request.bearer_auth(t),
            None => request,
        };
        let response = request
            .timeout(DEADLINE)
            .send()
            .await
            .map_err(|e| format!("calling the server: {e}"))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        Err(match (body["error"].as_str(), body["message"].as_str()) {
            (Some(code), Some(message)) => format!("{code}: {message}"),
            _ => format!("the server answered {status}"),
        })
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
        let request = self.http.get(format!("{}{path}", self.base)).query(query);
        let response = self.send(request).await?;
        response.json().await.map_err(|e| format!("reply: {e}"))
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let request = self.http.post(format!("{}{path}", self.base)).json(body);
        let response = self.send(request).await?;
        response.json().await.map_err(|e| format!("reply: {e}"))
    }

    async fn bytes(&self, path: &str) -> Result<Vec<u8>, String> {
        let response = self
            .send(self.http.get(format!("{}{path}", self.base)))
            .await?;
        let body = response.bytes().await.map_err(|e| format!("reply: {e}"))?;
        Ok(body.to_vec())
    }

    async fn put_blob(&self, repo: &str, data: Vec<u8>) -> Result<String, String> {
        let request = self
            .http
            .post(format!("{}/repos/{repo}/blobs", self.base))
            .body(data);
        let reply: Value = self
            .send(request)
            .await?
            .json()
            .await
            .map_err(|e| format!("reply: {e}"))?;
        text(&reply["id"])
    }

    /// The changeset `at` names (default `main`) and the root tree it leaves.
    async fn resolve(&self, repo: &str, at: Option<&str>) -> Result<(String, String), String> {
        let at = at.unwrap_or("main");
        let reply = if is_id(at) {
            let cs = self
                .get(&format!("/repos/{repo}/changesets/{at}"), &[])
                .await?;
            (text(&cs["id"])?, text(&cs["root_tree"])?)
        } else {
            let r = self.get(&format!("/repos/{repo}/refs/{at}"), &[]).await?;
            (text(&r["target"])?, text(&r["root_tree"])?)
        };
        Ok(reply)
    }

    /// The entry at `path` below the tree `root`: the root itself for an
    /// empty path, else `{"type": "tree" | "blob", "id", …}` as the API
    /// lists it. One tree read per path component.
    async fn walk(&self, repo: &str, root: &str, path: &str) -> Result<Value, String> {
        let mut entry = json!({"type": "tree", "id": root});
        for name in path.split('/').filter(|n| !n.is_empty()) {
            if entry["type"] != "tree" {
                return Err(format!("{path:?}: not a directory"));
            }
            let id = text(&entry["id"])?;
            let tree = self.get(&format!("/repos/{repo}/trees/{id}"), &[]).await?;
            entry = tree["entries"]
                .as_array()
                .and_then(|es| es.iter().find(|e| e["name"] == name).cloned())
                .ok_or_else(|| format!("{path:?}: no such file or directory"))?;
        }
        Ok(entry)
    }

    /// The file's text, or the directory's entries as JSON, at `path`.
    async fn read(&self, repo: &str, path: &str, at: Option<&str>) -> Answer {
        let (cs, root) = self.resolve(repo, at).await?;
        let entry = self.walk(repo, &root, path).await?;
        let id = text(&entry["id"])?;
        if entry["type"] == "tree" {
            let tree = self.get(&format!("/repos/{repo}/trees/{id}"), &[]).await?;
            return pretty(&json!({"changeset": cs, "path": path, "entries": tree["entries"]}));
        }
        let bytes = self.bytes(&format!("/repos/{repo}/blobs/{id}")).await?;
        String::from_utf8(bytes)
            .map_err(|e| format!("{path:?} is binary ({} bytes)", e.as_bytes().len()))
    }
}

fn is_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn text(v: &Value) -> Result<String, String> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("reply without a field the API documents: {v}"))
}

fn pretty(v: &Value) -> Answer {
    serde_json::to_string_pretty(v).map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Place {
    /// Repository name.
    repo: String,
    /// Path in the repository, `/`-separated, no leading `/`; empty for the root.
    #[serde(default)]
    path: String,
    /// Revision: `main`, `release/<x>` or a changeset id. Defaults to `main`.
    at: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Log {
    /// Repository name.
    repo: String,
    /// Revision to start from. Defaults to `main`.
    head: Option<String>,
    /// Only changesets that changed this path (a file or a directory).
    path: Option<String>,
    /// At most this many changesets, newest first (default 50, at most 1000).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Diff {
    /// Repository name.
    repo: String,
    /// Old revision. Defaults to `new`'s first parent.
    old: Option<String>,
    /// New revision.
    new: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Changeset {
    /// Repository name.
    repo: String,
    /// Revision.
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileWrite {
    /// Path to write, `/`-separated, no leading `/`.
    path: String,
    /// The file's new text. Left out, the file is deleted.
    content: Option<String>,
    /// Make the file executable.
    #[serde(default)]
    executable: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Commit {
    /// Repository name.
    repo: String,
    /// Revision the changeset goes on. Defaults to `main`.
    parent: Option<String>,
    /// What the change does and why.
    message: String,
    /// Author's name.
    author_name: String,
    /// Author's email.
    author_email: String,
    /// Files written or deleted; every other file stays as it is in `parent`.
    files: Vec<FileWrite>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Land {
    /// Repository name.
    repo: String,
    /// The changeset to land on `main`.
    head: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Requests {
    /// Repository name.
    repo: String,
    /// One change request by number; without it, the newest ones.
    id: Option<u64>,
    /// With no `id`: only requests in this status (`pending`, `landing`,
    /// `landed`, `needs_rebase`, `rejected`).
    status: Option<String>,
}

#[tool_router]
impl Scion {
    /// Serve the API at `base` (e.g. `http://127.0.0.1:8080/api/v1`), calling
    /// it with `token`; without one, only public repositories read.
    pub fn new(base: &str, token: Option<String>) -> Self {
        Self {
            api: Api {
                base: base.trim_end_matches('/').to_owned(),
                token,
                http: reqwest::Client::new(),
            },
        }
    }

    /// Serve streamable HTTP at `/mcp` on `listener` until it fails.
    pub async fn serve_http(self, listener: tokio::net::TcpListener) -> std::io::Result<()> {
        let service = StreamableHttpService::new(
            move || Ok(self.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        axum::serve(listener, axum::Router::new().nest_service("/mcp", service)).await
    }

    #[tool(
        description = "List the repositories the caller can read.",
        annotations(read_only_hint = true)
    )]
    async fn list_repos(&self) -> Answer {
        pretty(&self.api.get("/repos", &[]).await?)
    }

    #[tool(
        description = "List a directory at a revision: names, and for files their kind and size.",
        annotations(read_only_hint = true)
    )]
    async fn tree(&self, Parameters(p): Parameters<Place>) -> Answer {
        let (cs, root) = self.api.resolve(&p.repo, p.at.as_deref()).await?;
        let entry = self.api.walk(&p.repo, &root, &p.path).await?;
        if entry["type"] != "tree" {
            return Err(format!("{:?} is a file; read it with read_file", p.path));
        }
        let id = text(&entry["id"])?;
        let tree = self
            .api
            .get(&format!("/repos/{}/trees/{id}", p.repo), &[])
            .await?;
        pretty(&json!({"changeset": cs, "path": p.path, "entries": tree["entries"]}))
    }

    #[tool(
        description = "Read a file's text at a revision.",
        annotations(read_only_hint = true)
    )]
    async fn read_file(&self, Parameters(p): Parameters<Place>) -> Answer {
        self.api.read(&p.repo, &p.path, p.at.as_deref()).await
    }

    #[tool(
        description = "Changesets newest first along first parents, optionally only those that \
                       changed a path. Page by asking again from the last one's parent.",
        annotations(read_only_hint = true)
    )]
    async fn log(&self, Parameters(p): Parameters<Log>) -> Answer {
        let (head, _) = self.api.resolve(&p.repo, p.head.as_deref()).await?;
        let limit = p.limit.map(|n| n.to_string());
        let mut query = vec![("head", head.as_str())];
        if let Some(path) = &p.path {
            query.push(("path", path));
        }
        if let Some(limit) = &limit {
            query.push(("limit", limit));
        }
        pretty(
            &self
                .api
                .get(&format!("/repos/{}/log", p.repo), &query)
                .await?,
        )
    }

    #[tool(
        description = "For each run of lines of a file at a revision, the changeset that last \
                       changed it.",
        annotations(read_only_hint = true)
    )]
    async fn blame(&self, Parameters(p): Parameters<Place>) -> Answer {
        let (cs, _) = self.api.resolve(&p.repo, p.at.as_deref()).await?;
        let query = [("changeset", cs.as_str()), ("path", p.path.as_str())];
        pretty(
            &self
                .api
                .get(&format!("/repos/{}/blame", p.repo), &query)
                .await?,
        )
    }

    #[tool(
        description = "The files that differ between two revisions, with their old and new \
                       blob ids; read either side with read_file.",
        annotations(read_only_hint = true)
    )]
    async fn diff(&self, Parameters(p): Parameters<Diff>) -> Answer {
        let (new, _) = self.api.resolve(&p.repo, Some(&p.new)).await?;
        let old = if let Some(old) = &p.old {
            Some(self.api.resolve(&p.repo, Some(old)).await?.0)
        } else {
            let cs = self
                .api
                .get(&format!("/repos/{}/changesets/{new}", p.repo), &[])
                .await?;
            cs["parents"][0].as_str().map(str::to_owned)
        };
        let mut query = vec![("new", new.as_str())];
        if let Some(old) = &old {
            query.push(("old", old));
        }
        pretty(
            &self
                .api
                .get(&format!("/repos/{}/diff", p.repo), &query)
                .await?,
        )
    }

    #[tool(
        description = "One changeset: parents, author, message and the files it changes \
                       relative to its first parent.",
        annotations(read_only_hint = true)
    )]
    async fn changeset(&self, Parameters(p): Parameters<Changeset>) -> Answer {
        let (id, _) = self.api.resolve(&p.repo, Some(&p.id)).await?;
        pretty(
            &self
                .api
                .get(&format!("/repos/{}/changesets/{id}", p.repo), &[])
                .await?,
        )
    }

    #[tool(
        description = "Write and delete files on top of a revision as one new changeset. \
                       Nothing moves: land the returned changeset to put it on main."
    )]
    async fn write_files_and_commit(&self, Parameters(p): Parameters<Commit>) -> Answer {
        let (parent, _) = self.api.resolve(&p.repo, p.parent.as_deref()).await?;
        let mut changes = Map::new();
        for f in p.files {
            let change = match f.content {
                None => json!({"change": "deleted"}),
                Some(content) => {
                    let size = content.len();
                    let id = self.api.put_blob(&p.repo, content.into_bytes()).await?;
                    let kind = if f.executable {
                        "executable"
                    } else {
                        "regular"
                    };
                    json!({"change": "tracked", "content": id, "kind": kind, "size": size})
                }
            };
            changes.insert(f.path, change);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();
        let cs = json!({
            "parents": [parent],
            "author": {"name": p.author_name, "email": p.author_email},
            "author_date": {"unix_secs": now, "tz_offset_minutes": 0},
            "message": p.message,
            "file_changes": changes,
        });
        let reply = self
            .api
            .post(&format!("/repos/{}/changesets", p.repo), &cs)
            .await?;
        pretty(
            &json!({"changeset": reply["id"], "parent": parent, "root_tree": reply["root_tree"]}),
        )
    }

    #[tool(
        description = "Submit a changeset as a change request and wait (up to 20 seconds) while \
                       the server rebases it onto main and lands it. The reply's status is \
                       landed, needs_rebase (with the conflicting paths) or still pending."
    )]
    async fn land(&self, Parameters(p): Parameters<Land>) -> Answer {
        let (head, _) = self.api.resolve(&p.repo, Some(&p.head)).await?;
        let body = json!({"head": head, "wait": true});
        pretty(
            &self
                .api
                .post(&format!("/repos/{}/change-requests", p.repo), &body)
                .await?,
        )
    }

    #[tool(
        description = "One change request by number, or the newest ones (optionally by status).",
        annotations(read_only_hint = true)
    )]
    async fn change_request(&self, Parameters(p): Parameters<Requests>) -> Answer {
        let reply = if let Some(id) = p.id {
            let path = format!("/repos/{}/change-requests/{id}", p.repo);
            self.api.get(&path, &[]).await?
        } else {
            let query: Vec<_> = p.status.iter().map(|s| ("status", s.as_str())).collect();
            let path = format!("/repos/{}/change-requests", p.repo);
            self.api.get(&path, &query).await?
        };
        pretty(&reply)
    }
}

/// `scion://<repo>/<path>@<revision>` split into its parts.
fn resource(uri: &str) -> Option<(&str, &str, &str)> {
    let rest = uri.strip_prefix("scion://")?;
    let (repo, rest) = rest.split_once('/').unwrap_or((rest, ""));
    let (path, at) = rest.rsplit_once('@')?;
    Some((repo, path.trim_end_matches('/'), at))
}

// The macro writes `async fn list_tools` without an await; the lint is new in
// clippy 1.99, so older toolchains do not know its name.
#[allow(unknown_lints, clippy::unused_async_trait_impl)]
#[tool_handler]
impl ServerHandler for Scion {
    fn get_info(&self) -> ServerConfig {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("scion-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Scion repositories. Read with tree, read_file, log, blame, diff and \
                 changeset; change files with write_files_and_commit, then land the new \
                 changeset on main with land. There are no branches: work is shared by \
                 changeset id.",
            )
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, ErrorData>> {
        let file = ResourceTemplate::new("scion://{repo}/{+path}@{revision}", "file")
            .with_description(
                "A file's text, or a directory's entries as JSON, at a revision (`main`, \
                 `release/<x>` or a changeset id).",
            );
        std::future::ready(Ok(ListResourceTemplatesResult::with_all_items(vec![file])))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri;
        let (repo, path, at) = resource(&uri).ok_or_else(|| {
            ErrorData::invalid_params(
                format!("{uri:?} is not scion://<repo>/<path>@<revision>"),
                None,
            )
        })?;
        let content = self
            .api
            .read(repo, path, Some(at))
            .await
            .map_err(|e| ErrorData::resource_not_found(format!("{uri}: {e}"), None))?;
        let contents = vec![ResourceContents::text(content, uri)];
        Ok(ReadResourceResult::new(contents).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_uris_split_into_repo_path_and_revision() {
        assert_eq!(
            resource("scion://demo/src/lib.rs@main"),
            Some(("demo", "src/lib.rs", "main"))
        );
        assert_eq!(
            resource("scion://demo/@release/1"),
            Some(("demo", "", "release/1"))
        );
        assert_eq!(resource("scion://demo@main"), None);
        assert_eq!(resource("file:///etc/passwd"), None);
    }
}
