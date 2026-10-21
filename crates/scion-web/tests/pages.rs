//! Every page on the memory stores: what it shows, who may see it, and how
//! many metadata calls it costs. A metadata call is a round trip to
//! Postgres; every page but the log makes at most five of them, whatever
//! the history behind it, and the log makes one more per changeset it lists
//! (the first-parent walk of `scion_core::log`, which the `Log` RPC shares).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use bytes::Bytes;
use scion_derived::{Blame, Deriver, PathHistory, RootTree, SkipList};
use scion_store::{
    ChangeRequest, ChangeRequestStatus, DerivedPointer, Grant, LandOutcome, MemoryStore,
    MetadataStore, ObjectStore, RefState, Role,
};
use scion_types::{
    BlobId, Changeset, ChangesetId, FileChange, FileType, Identity, RepoPath, Timestamp, Tree,
    TreeId,
};
use scion_web::{Acted, Action, Task};
use tokio::net::TcpListener;

const REPO: &str = "demo";

/// Counts every metadata call; batched reads count once, as they are one
/// round trip on Postgres.
#[derive(Debug, Default)]
struct Counting {
    inner: MemoryStore,
    calls: AtomicUsize,
}

impl Counting {
    fn meta(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
    /// The metadata calls so far, starting again from zero.
    fn take(&self) -> usize {
        self.calls.swap(0, Ordering::Relaxed)
    }
}

#[async_trait]
impl MetadataStore for Counting {
    async fn create_repo(&self, name: &str) -> scion_store::Result<()> {
        self.meta();
        self.inner.create_repo(name).await
    }
    async fn put_changesets(&self, batch: &[Changeset]) -> scion_store::Result<()> {
        self.meta();
        self.inner.put_changesets(batch).await
    }
    async fn get_changeset(
        &self,
        repo: &str,
        id: ChangesetId,
    ) -> scion_store::Result<Option<Changeset>> {
        self.meta();
        self.inner.get_changeset(repo, id).await
    }
    async fn get_changesets(
        &self,
        repo: &str,
        ids: &[ChangesetId],
    ) -> scion_store::Result<Vec<Option<Changeset>>> {
        self.meta();
        self.inner.get_changesets(repo, ids).await
    }
    async fn get_derived_many(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        ids: &[ChangesetId],
    ) -> scion_store::Result<Vec<Option<Vec<u8>>>> {
        self.meta();
        self.inner.get_derived_many(repo, ty, version, ids).await
    }
    async fn generation(&self, repo: &str, id: ChangesetId) -> scion_store::Result<Option<u64>> {
        self.meta();
        self.inner.generation(repo, id).await
    }
    async fn create_ref(
        &self,
        repo: &str,
        name: &str,
        target: ChangesetId,
    ) -> scion_store::Result<RefState> {
        self.meta();
        self.inner.create_ref(repo, name, target).await
    }
    async fn get_ref(&self, repo: &str, name: &str) -> scion_store::Result<Option<RefState>> {
        self.meta();
        self.inner.get_ref(repo, name).await
    }
    async fn move_ref(
        &self,
        repo: &str,
        name: &str,
        expected_version: u64,
        new_target: ChangesetId,
    ) -> scion_store::Result<RefState> {
        self.meta();
        self.inner
            .move_ref(repo, name, expected_version, new_target)
            .await
    }
    async fn list_refs(&self, repo: &str) -> scion_store::Result<Vec<(String, RefState)>> {
        self.meta();
        self.inner.list_refs(repo).await
    }
    async fn put_derived_many(
        &self,
        repo: &str,
        pointers: Vec<DerivedPointer>,
    ) -> scion_store::Result<()> {
        self.meta();
        self.inner.put_derived_many(repo, pointers).await
    }
    async fn get_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
    ) -> scion_store::Result<Option<Vec<u8>>> {
        self.meta();
        self.inner.get_derived(repo, ty, version, cs).await
    }
    async fn put_ingest_checkpoints(
        &self,
        repo: &str,
        done: &[(Vec<u8>, ChangesetId)],
    ) -> scion_store::Result<()> {
        self.meta();
        self.inner.put_ingest_checkpoints(repo, done).await
    }
    async fn ingest_checkpoints(
        &self,
        repo: &str,
    ) -> scion_store::Result<Vec<(Vec<u8>, ChangesetId)>> {
        self.meta();
        self.inner.ingest_checkpoints(repo).await
    }

    async fn submit_change_request(
        &self,
        repo: &str,
        head: ChangesetId,
        base: ChangesetId,
        submitter: &str,
    ) -> scion_store::Result<ChangeRequest> {
        self.meta();
        self.inner
            .submit_change_request(repo, head, base, submitter)
            .await
    }

    async fn get_change_request(
        &self,
        repo: &str,
        id: u64,
    ) -> scion_store::Result<Option<ChangeRequest>> {
        self.meta();
        self.inner.get_change_request(repo, id).await
    }

    async fn list_change_requests(
        &self,
        repo: &str,
        status: Option<ChangeRequestStatus>,
        limit: u32,
    ) -> scion_store::Result<Vec<ChangeRequest>> {
        self.meta();
        self.inner.list_change_requests(repo, status, limit).await
    }

    async fn find_landings(
        &self,
        repo: &str,
        changesets: &[ChangesetId],
    ) -> scion_store::Result<Vec<(ChangesetId, ChangeRequest)>> {
        self.meta();
        self.inner.find_landings(repo, changesets).await
    }

    async fn claim_next_change_request(
        &self,
        repo: &str,
    ) -> scion_store::Result<Option<ChangeRequest>> {
        self.meta();
        self.inner.claim_next_change_request(repo).await
    }

    async fn landing_change_request(
        &self,
        repo: &str,
    ) -> scion_store::Result<Option<ChangeRequest>> {
        self.meta();
        self.inner.landing_change_request(repo).await
    }

    async fn finish_change_request(
        &self,
        repo: &str,
        id: u64,
        outcome: LandOutcome,
    ) -> scion_store::Result<ChangeRequest> {
        self.meta();
        self.inner.finish_change_request(repo, id, outcome).await
    }

    async fn reject_change_request(
        &self,
        repo: &str,
        id: u64,
    ) -> scion_store::Result<ChangeRequest> {
        self.meta();
        self.inner.reject_change_request(repo, id).await
    }

    async fn repos_with_open_change_requests(&self) -> scion_store::Result<Vec<String>> {
        self.meta();
        self.inner.repos_with_open_change_requests().await
    }

    async fn put_grant(&self, repo: &str, grant: &Grant) -> scion_store::Result<()> {
        self.meta();
        self.inner.put_grant(repo, grant).await
    }

    async fn delete_grant(
        &self,
        repo: &str,
        principal: &str,
        path_prefix: &str,
    ) -> scion_store::Result<bool> {
        self.meta();
        self.inner.delete_grant(repo, principal, path_prefix).await
    }

    async fn grants_for(&self, repo: &str, principal: &str) -> scion_store::Result<Vec<Grant>> {
        self.meta();
        self.inner.grants_for(repo, principal).await
    }

    async fn grants(&self, repo: &str) -> scion_store::Result<Vec<Grant>> {
        self.meta();
        self.inner.grants(repo).await
    }
    async fn repos_of(&self, principal: &str) -> scion_store::Result<Vec<String>> {
        self.meta();
        self.inner.repos_of(principal).await
    }
}

#[async_trait]
impl ObjectStore for Counting {
    async fn put_blob(&self, bytes: Bytes) -> scion_store::Result<BlobId> {
        self.inner.put_blob(bytes).await
    }
    async fn get_blob(&self, id: BlobId) -> scion_store::Result<Option<Bytes>> {
        self.inner.get_blob(id).await
    }
    async fn has_blobs(&self, ids: &[BlobId]) -> scion_store::Result<Vec<bool>> {
        self.inner.has_blobs(ids).await
    }
    async fn put_tree(&self, tree: &Tree) -> scion_store::Result<TreeId> {
        self.inner.put_tree(tree).await
    }
    async fn get_tree(&self, id: TreeId) -> scion_store::Result<Option<Tree>> {
        self.inner.get_tree(id).await
    }
}

fn changeset(
    repo: &str,
    parents: Vec<ChangesetId>,
    files: &[(&str, &[u8])],
    msg: &str,
) -> Changeset {
    Changeset {
        repo: repo.into(),
        parents,
        author: Identity::new("Ada", "ada@example.com"),
        author_date: Timestamp::new(1_791_969_120, 180),
        committer: None,
        committer_date: None,
        message: msg.into(),
        file_changes: files
            .iter()
            .map(|(path, content)| {
                let change = FileChange::Tracked {
                    content: BlobId::hash(content),
                    kind: FileType::Regular,
                    size: content.len() as u64,
                    copy_from: None,
                };
                (RepoPath::new(*path).unwrap(), change)
            })
            .collect::<BTreeMap<_, _>>(),
        extra: BTreeMap::new(),
    }
}

/// Store `cs` with its blobs and derive what pages read, as
/// `CreateChangeset` and a first blame would.
async fn commit(store: &Counting, cs: &Changeset, files: &[(&str, &[u8])]) -> ChangesetId {
    for (_, content) in files {
        store
            .put_blob(Bytes::copy_from_slice(content))
            .await
            .unwrap();
    }
    let id = store.put_changeset(cs).await.unwrap();
    let deriver = Deriver::new(&cs.repo, store, store);
    deriver.derive::<RootTree>(id).await.unwrap();
    deriver.derive::<SkipList>(id).await.unwrap();
    deriver.derive::<PathHistory>(id).await.unwrap();
    for (path, _) in files {
        Blame::for_path(&deriver, id, &RepoPath::new(*path).unwrap())
            .await
            .unwrap();
    }
    id
}

fn grant(principal: &str, path_prefix: &str) -> Grant {
    Grant {
        principal: principal.into(),
        path_prefix: path_prefix.into(),
        role: Role::Reader,
    }
}

struct Site {
    store: Arc<Counting>,
    base: String,
    first: ChangesetId,
    second: ChangesetId,
    /// Every task the pages asked for: who, in which repository, what.
    acted: Arc<Mutex<Vec<(String, String, Task)>>>,
}

impl Site {
    /// The change-request actions asked for so far: who, on which request,
    /// what.
    fn actions(&self) -> Vec<(String, u64, Action)> {
        self.acted
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(who, _, task)| match task {
                Task::Request(cr, action) => Some((who.clone(), cr.id, *action)),
                _ => None,
            })
            .collect()
    }
}

/// Stands in for the server: records each task, refuses `sp:docs` as the
/// server would, lands as request 99, finds `src/lib.rs` conflicting and
/// does everything else.
fn act(acted: Arc<Mutex<Vec<(String, String, Task)>>>) -> scion_web::Act {
    Arc::new(move |principal, repo, task| {
        let refused = principal == "sp:docs";
        acted.lock().unwrap().push((principal, repo, task.clone()));
        Box::pin(async move {
            if refused {
                return Err((StatusCode::FORBIDDEN, "sp:docs may not".to_owned()));
            }
            Ok(match task {
                Task::Request(_, Action::Land) => Acted::Submitted(99),
                Task::Request(_, Action::Rebase) => Acted::Conflicts(vec!["src/lib.rs".into()]),
                Task::Request(_, Action::Reject) => Acted::Rejected,
                _ => Acted::Done,
            })
        })
    })
}

/// `demo`: a README, `src/lib.rs` and, from the second changeset,
/// `docs/guide.md`; a change request that needs a rebase. `sp:ada` reads it
/// all, `sp:docs` only `docs/`.
async fn site() -> Site {
    let store = Arc::new(Counting::default());
    store.create_repo(REPO).await.unwrap();
    let files: &[(&str, &[u8])] = &[
        (
            "README.md",
            b"# Demo\n\nHello <b>there</b>, see [the guide](docs/guide.md).\n",
        ),
        ("src/lib.rs", b"pub fn one() -> u8 {\n    1\n}\n"),
    ];
    let first = changeset(REPO, vec![], files, "Start the demo");
    let first = commit(&store, &first, files).await;
    let files: &[(&str, &[u8])] = &[
        ("src/lib.rs", b"pub fn one() -> u8 {\n    2\n}\n"),
        ("docs/guide.md", b"Read me.\n"),
    ];
    let second = changeset(REPO, vec![first], files, "Fix one\n\nIt said one.");
    let second = commit(&store, &second, files).await;
    store.create_ref(REPO, "main", second).await.unwrap();
    store.create_ref(REPO, "release/1", first).await.unwrap();
    store.put_grant(REPO, &grant("sp:ada", "")).await.unwrap();
    store
        .put_grant(REPO, &grant("sp:docs", "docs"))
        .await
        .unwrap();
    let cr = store
        .submit_change_request(REPO, second, first, "sp:ada")
        .await
        .unwrap();
    store.claim_next_change_request(REPO).await.unwrap();
    store
        .finish_change_request(
            REPO,
            cr.id,
            LandOutcome::NeedsRebase(vec!["src/lib.rs".into()]),
        )
        .await
        .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let verify = |token: &str| {
        token
            .strip_prefix("token-")
            .map(str::to_owned)
            .ok_or_else(|| "not a token".to_owned())
    };
    // GitHub signs in `github:octocat` for the code `good`.
    let nonces = Arc::new(AtomicUsize::new(0));
    let login = scion_web::Login {
        authorize: "https://github.example/login/oauth/authorize?client_id=app".into(),
        nonce: Arc::new(move || format!("{:064x}", nonces.fetch_add(1, Ordering::Relaxed))),
        exchange: Arc::new(|code| {
            Box::pin(async move {
                match code.as_str() {
                    "good" => Ok(("token-github:octocat".to_owned(), Duration::from_secs(60))),
                    _ => Err("GitHub: bad_verification_code".to_owned()),
                }
            })
        }),
    };
    let acted = Arc::default();
    tokio::spawn(scion_web::serve(
        listener,
        store.clone(),
        store.clone(),
        Arc::new(verify),
        Some(login),
        act(Arc::clone(&acted)),
        axum::Router::new(),
    ));
    Site {
        store,
        base,
        first,
        second,
        acted,
    }
}

/// GET `path` as `principal`: the status and the page.
async fn get(site: &Site, principal: Option<&str>, path: &str) -> (u16, String) {
    let mut request = reqwest::Client::new().get(format!("{}{path}", site.base));
    if let Some(p) = principal {
        request = request.bearer_auth(format!("token-{p}"));
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

#[tokio::test]
async fn every_page_shows_its_content_within_five_metadata_calls() {
    let site = site().await;
    let (first, second) = (site.first.to_string(), site.second.to_string());
    let pages: Vec<(String, Vec<&str>)> = vec![
        ("/".into(), vec![">demo<"]),
        (
            "/demo".into(),
            vec![
                "<h1>Demo</h1>",
                "&lt;b&gt;there&lt;/b&gt;",
                "path=docs%2Fguide.md",
                ">release/1<",
                ">src/<",
            ],
        ),
        (
            "/demo/tree?path=src".into(),
            vec![">lib.rs<", "History of src/"],
        ),
        ("/demo/tree?path=README.md".into(), vec!["# Demo", "Blame"]),
        (
            "/demo/tree?at=release%2F1".into(),
            vec![">README.md<", ">src/<"],
        ),
        (
            "/demo/log".into(),
            vec![
                "Fix one",
                "Start the demo",
                "Ada",
                "2026-10-14 12:12:00 +0300",
                &second[..12],
            ],
        ),
        ("/demo/log?path=docs%2Fguide.md".into(), vec!["Fix one"]),
        (
            "/demo/blame?path=README.md".into(),
            vec![&first[..12], "Hello &lt;b&gt;there"],
        ),
        (
            format!("/demo/changeset/{second}"),
            vec![
                "It said one.",
                "2 files changed",
                "docs/guide.md",
                "src/lib.rs",
                "<span class=\"del\">-    1</span>",
                "<span class=\"add\">+    2</span>",
                &first,
            ],
        ),
        (
            "/demo/requests".into(),
            vec!["#1", "needs_rebase", "sp:ada"],
        ),
        (
            "/demo/requests/1".into(),
            vec!["Conflicts with main", "src/lib.rs", &second],
        ),
    ];
    for (path, wants) in pages {
        site.store.take();
        let (status, page) = get(&site, Some("sp:ada"), &path).await;
        assert_eq!(status, 200, "{path}: {page}");
        for want in wants {
            assert!(page.contains(want), "{path} lacks {want:?}: {page}");
        }
        let calls = site.store.take();
        assert!(calls <= 5, "{path} made {calls} metadata calls");
    }
}

#[tokio::test]
async fn blame_of_a_changed_line_names_the_changeset_that_changed_it() {
    let site = site().await;
    let (status, page) = get(&site, Some("sp:ada"), "/demo/blame?path=src%2Flib.rs").await;
    assert_eq!(status, 200, "{page}");
    let first = page.find(&site.first.to_string()[..12]).expect("first");
    let second = page.find(&site.second.to_string()[..12]).expect("second");
    assert!(
        first < second,
        "line 1 is from the first changeset, line 2 the second"
    );
}

#[tokio::test]
async fn pages_need_a_token_and_a_grant() {
    let site = site().await;
    let (status, page) = get(&site, None, "/demo").await;
    assert_eq!(status, 401, "{page}");
    assert!(page.contains("is not public: sign in at /login"), "{page}");
    assert!(page.contains("Authorization: Bearer"), "{page}");
    let response = reqwest::Client::new()
        .get(format!("{}/demo", site.base))
        .bearer_auth("forged")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);

    for path in ["/demo", "/missing", "/demo/log", "/demo/requests/1"] {
        let (status, page) = get(&site, Some("sp:nobody"), path).await;
        assert_eq!(status, 403, "{path}: {page}");
        assert!(page.contains("sp:nobody is not a reader"), "{page}");
    }
    let (status, page) = get(&site, Some("sp:nobody"), "/").await;
    assert_eq!(status, 200);
    assert!(page.contains("holds no grant"), "{page}");
    let (status, page) = get(&site, None, "/").await;
    assert_eq!(status, 200);
    assert!(page.contains("No repository is public"), "{page}");
    assert!(page.contains("href=\"/login\""), "{page}");
}

#[tokio::test]
async fn a_directory_reader_sees_log_blame_and_diffs_under_its_directory_only() {
    let site = site().await;
    let docs = Some("sp:docs");
    for path in ["/demo/log?path=src", "/demo/blame?path=src%2Flib.rs"] {
        let (status, page) = get(&site, docs, path).await;
        assert_eq!(status, 403, "{path}: {page}");
        assert!(page.contains("may not read src"), "{page}");
    }
    let (status, page) = get(&site, docs, "/demo/log?path=docs").await;
    assert_eq!(status, 200, "{page}");
    let (status, page) = get(&site, docs, &format!("/demo/changeset/{}", site.second)).await;
    assert_eq!(status, 200, "{page}");
    assert!(page.contains("docs/guide.md"), "{page}");
    assert!(!page.contains("src/lib.rs"), "{page}");
    let (_, page) = get(&site, docs, "/").await;
    assert!(page.contains(">demo<"), "{page}");
}

#[tokio::test]
async fn bad_locations_are_refused_by_name() {
    let site = site().await;
    for (path, status, says) in [
        ("/demo/tree?path=nope", 404, "no nope"),
        (
            "/demo/tree?at=topic",
            400,
            "neither a changeset id nor a ref",
        ),
        ("/demo/tree?at=release%2F9", 404, "no ref"),
        ("/demo/tree?path=..%2Fx", 400, "path"),
        ("/demo/blame", 400, "needs a file path"),
        ("/demo/changeset/zz", 400, "changeset"),
        ("/demo/requests/9", 404, "no change request 9"),
    ] {
        let (got, page) = get(&site, Some("sp:ada"), path).await;
        assert_eq!(got, status, "{path}: {page}");
        assert!(page.contains(says), "{path}: {page}");
    }
}

#[tokio::test]
async fn a_long_log_pages_from_the_last_parent() {
    let site = site().await;
    let mut head = site.second;
    for i in 0..60 {
        let path = format!("n/{i}");
        let files: &[(&str, &[u8])] = &[(&path, b"x")];
        let cs = changeset(REPO, vec![head], files, &format!("Step {i}"));
        head = commit(&site.store, &cs, files).await;
    }
    let main = site.store.get_ref(REPO, "main").await.unwrap().unwrap();
    site.store
        .move_ref(REPO, "main", main.version, head)
        .await
        .unwrap();

    let (_, page) = get(&site, Some("sp:ada"), "/demo/log").await;
    assert!(
        page.contains("Step 59") && page.contains("Step 10"),
        "{page}"
    );
    assert!(!page.contains("Step 9<"), "a page lists 50: {page}");
    let (link, _) = page.rsplit_once(">Older<").expect("an Older link");
    let (_, older) = link.rsplit_once("href=\"").unwrap();
    let older = older.trim_end_matches('"').replace("&amp;", "&");
    let (_, page) = get(&site, Some("sp:ada"), &older).await;
    assert!(
        page.contains("Step 9<") && page.contains("Start the demo"),
        "{page}"
    );
    assert!(!page.contains("Step 10<"), "{page}");
}

/// GET `path` with `cookie`, not following redirects: the response.
async fn get_with_cookie(site: &Site, cookie: &str, path: &str) -> reqwest::Response {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(format!("{}{path}", site.base))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
}

/// The `name=value` pairs of a response's `Set-Cookie` headers, each with
/// its attributes.
fn set_cookies(response: &reqwest::Response) -> Vec<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_public_repository_reads_without_a_session() {
    let site = site().await;
    site.store
        .put_grant(REPO, &grant(scion_web::ANYONE, ""))
        .await
        .unwrap();
    let (status, page) = get(&site, None, "/").await;
    assert_eq!(status, 200);
    assert!(
        page.contains(">demo<") && page.contains("Sign in with GitHub"),
        "{page}"
    );
    for (path, want) in [
        ("/demo", "<h1>Demo</h1>"),
        ("/demo/log?path=src", "Fix one"),
        ("/demo/requests", "needs_rebase"),
    ] {
        site.store.take();
        let (status, page) = get(&site, None, path).await;
        assert_eq!(status, 200, "{path}: {page}");
        assert!(page.contains(want), "{path}: {page}");
        let calls = site.store.take();
        assert!(calls <= 5, "{path} made {calls} metadata calls");
    }
    // A principal without grants of its own reads it too.
    let (status, page) = get(&site, Some("sp:nobody"), "/demo/requests").await;
    assert_eq!(status, 200, "{page}");
    let (_, page) = get(&site, Some("sp:nobody"), "/").await;
    assert!(
        page.contains(">demo<") && page.contains("Signed in as"),
        "{page}"
    );
}

#[tokio::test]
async fn a_public_directory_shows_only_its_history_without_a_session() {
    let site = site().await;
    site.store
        .put_grant(REPO, &grant(scion_web::ANYONE, "docs"))
        .await
        .unwrap();
    let (status, page) = get(&site, None, "/demo/log?path=docs").await;
    assert_eq!(status, 200, "{page}");
    let (status, page) = get(&site, None, "/demo/blame?path=src%2Flib.rs").await;
    assert_eq!(status, 403, "{page}");
    assert!(page.contains("anyone may not read src/lib.rs"), "{page}");
    // `sp:docs` reads docs/ by its own grant, and nothing more through anyone's.
    let (status, _) = get(&site, Some("sp:docs"), "/demo/log?path=src").await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn signing_in_with_github_sets_a_session_that_reads_change_requests() {
    let site = site().await;
    site.store
        .put_grant(REPO, &grant("github:octocat", ""))
        .await
        .unwrap();
    let started = get_with_cookie(&site, "", "/login").await;
    assert_eq!(started.status().as_u16(), 303);
    let to = started.headers()["location"].to_str().unwrap().to_owned();
    let state = to
        .strip_prefix("https://github.example/login/oauth/authorize?client_id=app&state=")
        .expect(&to)
        .to_owned();
    let cookies = set_cookies(&started);
    assert_eq!(cookies.len(), 1, "{cookies:?}");
    assert!(
        cookies[0].starts_with(&format!("scion_login={state}; Path=/login; Max-Age=600;"))
            && cookies[0].ends_with("HttpOnly; Secure; SameSite=Lax"),
        "{cookies:?}"
    );

    let state_cookie = format!("scion_login={state}");
    for (cookie, query, status, says) in [
        (
            "",
            format!("code=good&state={state}"),
            400,
            "not started from this browser",
        ),
        (
            state_cookie.as_str(),
            "code=good&state=other".to_owned(),
            400,
            "not started",
        ),
        (
            state_cookie.as_str(),
            format!("code=bad&state={state}"),
            502,
            "bad_verification_code",
        ),
        (
            state_cookie.as_str(),
            "error=access_denied&error_description=The+user+said+no".to_owned(),
            403,
            "The user said no",
        ),
    ] {
        let response = get_with_cookie(&site, cookie, &format!("/login/callback?{query}")).await;
        assert_eq!(response.status().as_u16(), status, "{query}");
        assert!(set_cookies(&response).is_empty(), "{query}");
        assert!(response.text().await.unwrap().contains(says), "{query}");
    }

    let back = get_with_cookie(
        &site,
        &format!("other=1; {state_cookie}"),
        &format!("/login/callback?code=good&state={state}"),
    )
    .await;
    assert_eq!(back.status().as_u16(), 303);
    assert_eq!(back.headers()["location"], "/");
    let cookies = set_cookies(&back);
    assert_eq!(
        cookies,
        [
            "scion_session=token-github:octocat; Path=/; Max-Age=60; HttpOnly; Secure; SameSite=Lax",
            "scion_login=; Path=/login; Max-Age=0; HttpOnly; Secure; SameSite=Lax",
        ]
    );

    let session = "scion_session=token-github:octocat";
    let requests = get_with_cookie(&site, session, "/demo/requests").await;
    assert_eq!(requests.status().as_u16(), 200);
    let page = requests.text().await.unwrap();
    assert!(
        page.contains("needs_rebase") && page.contains("#1"),
        "{page}"
    );
    let home = get_with_cookie(&site, session, "/").await;
    let page = home.text().await.unwrap();
    assert!(
        page.contains("Signed in as <strong>github:octocat</strong>"),
        "{page}"
    );
    assert!(page.contains(">demo<"), "{page}");

    // A session that no longer verifies is no session.
    let expired = get_with_cookie(&site, "scion_session=expired", "/demo").await;
    assert_eq!(expired.status().as_u16(), 401);

    let out = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{}/logout", site.base))
        .header("cookie", session)
        .send()
        .await
        .unwrap();
    assert_eq!(out.status().as_u16(), 303);
    assert_eq!(
        set_cookies(&out),
        ["scion_session=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax"]
    );
}

/// POST `path` as `principal`, from a page of `origin` if given, without
/// following a redirect.
async fn post(
    site: &Site,
    principal: Option<&str>,
    origin: Option<&str>,
    path: &str,
) -> reqwest::Response {
    let mut request = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{}{path}", site.base));
    if let Some(p) = principal {
        request = request.bearer_auth(format!("token-{p}"));
    }
    if let Some(o) = origin {
        request = request.header("origin", o);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn a_change_request_page_offers_the_actions_of_the_callers_role() {
    let site = site().await;
    for (principal, role) in [("sp:bob", Role::Writer), ("sp:root", Role::Admin)] {
        let g = Grant {
            principal: principal.into(),
            path_prefix: String::new(),
            role,
        };
        site.store.put_grant(REPO, &g).await.unwrap();
    }
    let action = |a: &str| format!("action=\"/demo/requests/1/{a}\"");
    // Request 1 needs a rebase; `sp:ada` submitted it and only reads.
    for (principal, offered) in [
        ("sp:ada", &["reject"][..]),
        ("sp:bob", &["rebase", "land"]),
        ("sp:root", &["rebase", "land", "reject"]),
        ("sp:docs", &[]),
    ] {
        let (status, page) = get(&site, Some(principal), "/demo/requests/1").await;
        assert_eq!(status, 200, "{page}");
        for a in ["rebase", "land", "reject"] {
            assert_eq!(
                page.contains(&action(a)),
                offered.contains(&a),
                "{principal} {a}: {page}"
            );
        }
    }
    site.store.take();

    // A land goes to the new request, a reject back to this one.
    let landed = post(&site, Some("sp:bob"), None, "/demo/requests/1/land").await;
    assert_eq!(landed.status().as_u16(), 303);
    assert_eq!(landed.headers()["location"], "/demo/requests/99");
    let rejected = post(
        &site,
        Some("sp:ada"),
        Some(&site.base),
        "/demo/requests/1/reject",
    )
    .await;
    assert_eq!(rejected.status().as_u16(), 303);
    assert_eq!(rejected.headers()["location"], "/demo/requests/1");
    // A rebase shows where the head stands on the request's page.
    let rebased = post(&site, Some("sp:bob"), None, "/demo/requests/1/rebase").await;
    assert_eq!(rebased.status().as_u16(), 200);
    let page = rebased.text().await.unwrap();
    assert!(
        page.contains("Still conflicts with main") && page.contains("scn update"),
        "{page}"
    );
    assert!(
        site.store.take() <= 3 * 2,
        "each action reads the grants and the request"
    );

    // The server's refusal is shown with its status.
    let refused = post(&site, Some("sp:docs"), None, "/demo/requests/1/reject").await;
    assert_eq!(refused.status().as_u16(), 403);
    assert!(refused.text().await.unwrap().contains("sp:docs may not"));
    assert_eq!(
        site.actions(),
        [
            ("sp:bob".to_owned(), 1, Action::Land),
            ("sp:ada".to_owned(), 1, Action::Reject),
            ("sp:bob".to_owned(), 1, Action::Rebase),
            ("sp:docs".to_owned(), 1, Action::Reject),
        ]
    );

    // Refused before the server is asked: another site's page, a visitor
    // or a principal that only reads what anyone does, an unknown action or
    // request.
    site.store
        .put_grant(REPO, &grant(scion_web::ANYONE, ""))
        .await
        .unwrap();
    for (principal, origin, path, status) in [
        (
            Some("sp:bob"),
            Some("https://evil.example"),
            "/demo/requests/1/land",
            403,
        ),
        (None, None, "/demo/requests/1/land", 401),
        (Some("sp:eve"), None, "/demo/requests/1/land", 403),
        (Some("sp:bob"), None, "/demo/requests/1/merge", 404),
        (Some("sp:bob"), None, "/demo/requests/7/land", 404),
    ] {
        let response = post(&site, principal, origin, path).await;
        assert_eq!(response.status().as_u16(), status, "{path} {origin:?}");
    }
    assert_eq!(site.actions().len(), 4);
}

/// POST the form `body` to `path` as `principal`, without following a
/// redirect.
async fn post_form(
    site: &Site,
    principal: Option<&str>,
    path: &str,
    body: &str,
) -> reqwest::Response {
    let mut request = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{}{path}", site.base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body.to_owned());
    if let Some(p) = principal {
        request = request.bearer_auth(format!("token-{p}"));
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn an_admin_creates_repositories_and_manages_access_from_the_pages() {
    let site = site().await;
    let root = Grant {
        principal: "sp:root".into(),
        path_prefix: String::new(),
        role: Role::Admin,
    };
    site.store.put_grant(REPO, &root).await.unwrap();

    // Settings are for an admin of the whole repository.
    let (status, page) = get(&site, Some("sp:ada"), "/demo/settings").await;
    assert_eq!(status, 403, "{page}");
    assert!(page.contains("sp:ada is not an admin of all of"), "{page}");
    site.store.take();
    let (status, page) = get(&site, Some("sp:root"), "/demo/settings").await;
    assert_eq!(status, 200, "{page}");
    assert!(site.store.take() <= 5);
    for want in [
        "Private",
        "Make public",
        "sp:ada",
        "docs/",
        "the whole repository",
    ] {
        assert!(page.contains(want), "lacks {want:?}: {page}");
    }

    let changes = [
        ("principal=sp%3Abob&role=writer&path=src%2F", 303),
        ("principal=anyone&role=reader", 303),
        ("principal=anyone", 400),
    ];
    for (body, status) in changes {
        let response = post_form(&site, Some("sp:root"), "/demo/settings/grant", body).await;
        assert_eq!(response.status().as_u16(), status, "{body}");
    }
    let revoked = post_form(
        &site,
        Some("sp:root"),
        "/demo/settings/revoke",
        "principal=sp%3Adocs&path=docs",
    )
    .await;
    assert_eq!(revoked.status().as_u16(), 303);
    assert_eq!(revoked.headers()["location"], "/demo/settings");
    for (who, path, status) in [
        (Some("sp:root"), "/demo/settings/frob", 404),
        (None, "/demo/settings/revoke", 401),
    ] {
        let response = post_form(&site, who, path, "principal=sp%3Aada").await;
        assert_eq!(response.status().as_u16(), status, "{path}");
    }

    // Anyone signed in is offered the form; the server decides.
    let (_, page) = get(&site, Some("sp:ada"), "/").await;
    assert!(page.contains("Create repository"), "{page}");
    let (_, page) = get(&site, None, "/").await;
    assert!(!page.contains("Create repository"), "{page}");
    let created = post_form(&site, Some("sp:root"), "/", "repo=+new-one+").await;
    assert_eq!(created.status().as_u16(), 303);
    assert_eq!(created.headers()["location"], "/new-one");
    let unnamed = post_form(&site, Some("sp:root"), "/", "repo=+").await;
    assert_eq!(unnamed.status().as_u16(), 400);

    let granted = |principal: &str, role: &str, path_prefix: &str| Task::Grant {
        principal: principal.into(),
        role: role.into(),
        path_prefix: path_prefix.into(),
    };
    let root = |repo: &str, task| ("sp:root".to_owned(), repo.to_owned(), task);
    assert_eq!(
        *site.acted.lock().unwrap(),
        [
            root(REPO, granted("sp:bob", "writer", "src")),
            root(REPO, granted("anyone", "reader", "")),
            root(
                REPO,
                Task::Revoke {
                    principal: "sp:docs".into(),
                    path_prefix: "docs".into()
                }
            ),
            root("new-one", Task::Init),
        ]
    );

    // Public once anyone reads it.
    site.store
        .put_grant(REPO, &grant(scion_web::ANYONE, ""))
        .await
        .unwrap();
    let (_, page) = get(&site, Some("sp:root"), "/demo/settings").await;
    assert!(
        page.contains("Public") && page.contains("Make private"),
        "{page}"
    );
}
