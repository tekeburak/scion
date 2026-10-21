//! Hermetic simulation of `scion-server`. One server runs on the memory
//! stores inside a single-threaded runtime, with no network: clients call
//! its gRPC services in process. A seeded scheduler picks, step after step,
//! which clients act and how (commit, land, update, merge, revert, or move
//! `main` directly to race the land worker), how long the land workers run
//! in between, which store call fails (a stop between creating the rebased
//! stack and the compare-and-swap of `main`, or between that and recording
//! the outcome), and when the server restarts (its runtime is dropped, so a
//! land worker stops wherever it was). The store yields to the scheduler
//! before calls as the seed says, so concurrent requests interleave
//! differently per seed and identically per run of the same seed.
//!
//! After the run, [`run`] checks that `main` only ever moved forward along
//! its first-parent line, that the change of every landed request is on
//! `main` exactly once, that no object, changeset or derived pointer was
//! overwritten with different bytes, and that no request was lost: after a
//! last restart every submitted request finishes.

#![forbid(unsafe_code)]

pub mod fuzz;
mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures_util::future::join_all;
use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::object_service_client::ObjectServiceClient;
use scion_proto::v1::{
    ChangeRequestStatus as WireStatus, CreateChangesetRequest, GetChangeRequestRequest,
    GetRefRequest, InitRepoRequest, MergeRequest, MoveRefRequest, PutBlobRequest, RebaseRequest,
    RevertRequest, SubmitChangeRequestRequest,
};
use scion_server::TokenKey;
use scion_store::{ChangeRequestStatus, Grant, MetadataStore, ObjectStore, Role, SERVER_GRANTS};
use scion_types::{
    BlobId, Changeset, ChangesetId, Encode, FileChange, FileType, Identity, RepoPath, Timestamp,
};
use tokio::runtime::Runtime;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::interceptor::InterceptedService;
use tonic::service::{Interceptor, Routes};
use tonic::{Code, Status};

use store::{Fault, SimStore};

/// The repository every run works in.
pub const REPO: &str = "sim";

/// Steps of one run.
const STEPS: u32 = 60;

/// Clients of one run.
const CLIENTS: usize = 4;

/// Principal of every client: a server admin, so it creates the repository
/// and may move `main` directly.
const PRINCIPAL: &str = "sp:sim";

/// The command line that replays `seed` alone.
#[must_use]
pub fn reproduce(seed: u64) -> String {
    format!("SCION_SIM_SEED={seed} cargo test -p scion-sim --test seeds")
}

/// A splitmix64 generator behind a lock: the scheduler and the store draw
/// from it in the order the single-threaded runtime polls them.
pub(crate) struct Rng(Mutex<u64>);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(Mutex::new(seed))
    }

    fn next(&self) -> u64 {
        let mut s = self.0.lock().expect("poisoned");
        *s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`.
    pub(crate) fn below(&self, n: u64) -> u64 {
        self.next() % n
    }
}

/// What a run produced; the same seed produces the same history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    /// `main`'s first-parent line at the end, newest first.
    pub main: Vec<ChangesetId>,
    /// `main` after every step.
    pub mains: Vec<ChangesetId>,
    /// Every request: id, final status and the changeset it landed as.
    pub requests: Vec<(u64, ChangeRequestStatus, Option<ChangesetId>)>,
}

/// Adds the run's token to every request.
#[derive(Clone)]
struct Auth(MetadataValue<Ascii>);

impl Interceptor for Auth {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, Status> {
        req.metadata_mut().insert("authorization", self.0.clone());
        Ok(req)
    }
}

type Channel = InterceptedService<Routes, Auth>;

/// One client's connection to the running server.
#[derive(Clone)]
struct Api {
    control: ControlServiceClient<Channel>,
    objects: ObjectServiceClient<Channel>,
}

/// One lifetime of the server: dropping it drops the runtime and with it
/// every land worker, wherever it was.
struct Server {
    rt: Runtime,
    api: Api,
}

impl Server {
    fn start(store: &Arc<SimStore>, key: &TokenKey, auth: &Auth) -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime starts");
        let routes = rt
            .block_on(scion_server::routes(
                store.clone(),
                store.clone(),
                key.clone(),
                None,
            ))
            .expect("the memory store answers");
        let channel = InterceptedService::new(routes, auth.clone());
        Self {
            rt,
            api: Api {
                control: ControlServiceClient::new(channel.clone()),
                objects: ObjectServiceClient::new(channel),
            },
        }
    }
}

/// A client's working state: its head and the request it waits on.
struct Client {
    index: usize,
    head: ChangesetId,
    /// Changesets it made so far; numbers its messages.
    made: u32,
    landing: Option<u64>,
}

/// What a client does in a step, drawn before the step runs.
#[derive(Debug, Clone, Copy)]
enum Op {
    /// Change one file on top of the head: its own (`c<i>/f<n>`) or one
    /// every client changes (`shared/s<n>`).
    Commit { shared: bool, file: u64 },
    /// Submit the head as a change request.
    Land,
    /// Ask about the request it waits on, and move on once finished.
    Poll,
    /// Rebase the head onto `main`; on a conflict, start over at `main`.
    Update,
    /// Merge another client's head into its own.
    Merge { theirs: ChangesetId },
    /// Undo the head's own change on top of it.
    Revert,
    /// Move `main` directly to a new changeset on top of it, racing the
    /// land worker's compare-and-swap.
    Race,
}

/// Failures a client expects: refusals of what it asked, and errors the
/// simulation injected.
fn expected(st: &Status) -> bool {
    st.message().contains("injected")
        || matches!(
            st.code(),
            Code::FailedPrecondition | Code::NotFound | Code::InvalidArgument | Code::Aborted
        )
}

fn id(bytes: &[u8]) -> ChangesetId {
    ChangesetId::try_from_slice(bytes).expect("the server sends 32-byte ids")
}

impl Client {
    /// A changeset of this client on top of `parent` that writes `content`
    /// to `path`, numbered `self.made`.
    fn changeset(&self, parent: ChangesetId, path: &str, content: &[u8], what: &str) -> Changeset {
        let c = self.index;
        Changeset {
            repo: REPO.into(),
            parents: vec![parent],
            author: Identity::new(format!("c{c}"), format!("c{c}@sim")),
            author_date: Timestamp::new(i64::from(self.made), 0),
            committer: None,
            committer_date: None,
            message: format!("c{c}-{} {what}", self.made),
            file_changes: BTreeMap::from([(
                RepoPath::new(path).expect("simulated paths are valid"),
                FileChange::Tracked {
                    content: BlobId::hash(content),
                    kind: FileType::Regular,
                    size: content.len() as u64,
                    copy_from: None,
                },
            )]),
            extra: BTreeMap::new(),
        }
    }

    /// Store `content` and a changeset writing it to `path` on `parent`.
    async fn create(
        &mut self,
        api: &mut Api,
        parent: ChangesetId,
        path: &str,
        what: &str,
    ) -> Result<ChangesetId, Status> {
        self.made += 1;
        let content = format!("c{}-{}\n", self.index, self.made).into_bytes();
        let cs = self.changeset(parent, path, &content, what);
        api.objects
            .put_blob(PutBlobRequest {
                data: content,
                repo: REPO.into(),
            })
            .await?;
        api.control
            .create_changeset(CreateChangesetRequest {
                changeset: cs.encode_to_vec(),
            })
            .await?;
        Ok(cs.id())
    }

    async fn main(api: &mut Api) -> Result<(ChangesetId, u64), Status> {
        let main = api
            .control
            .get_ref(GetRefRequest {
                repo: REPO.into(),
                name: "main".into(),
            })
            .await?
            .into_inner();
        Ok((id(&main.target), main.version))
    }

    /// Run `op`; returns the request it submitted, if any.
    #[allow(clippy::too_many_lines)] // one arm per operation
    async fn act(&mut self, mut api: Api, op: Op) -> Result<Option<u64>, Status> {
        let c = self.index;
        let author = |me: &Self| (format!("c{c}"), format!("c{c}@sim"), i64::from(me.made));
        match op {
            Op::Commit { shared, file } => {
                let path = if shared {
                    format!("shared/s{file}")
                } else {
                    format!("c{c}/f{file}")
                };
                self.head = self.create(&mut api, self.head, &path, "commit").await?;
            }
            Op::Land => {
                let cr = api
                    .control
                    .submit_change_request(SubmitChangeRequestRequest {
                        repo: REPO.into(),
                        head: self.head.as_bytes().to_vec(),
                        wait: false,
                    })
                    .await?
                    .into_inner();
                self.landing = Some(cr.id);
                return Ok(Some(cr.id));
            }
            Op::Poll => {
                let Some(request) = self.landing else {
                    return Ok(None);
                };
                let cr = api
                    .control
                    .get_change_request(GetChangeRequestRequest {
                        repo: REPO.into(),
                        id: request,
                        wait: false,
                    })
                    .await?
                    .into_inner();
                match WireStatus::try_from(cr.status) {
                    Ok(WireStatus::Landed) => {
                        self.head = id(cr.landed.as_deref().unwrap_or_default());
                        self.landing = None;
                    }
                    Ok(WireStatus::NeedsRebase) => self.landing = None,
                    Ok(WireStatus::Rejected) => {
                        self.head = Self::main(&mut api).await?.0;
                        self.landing = None;
                    }
                    _ => {}
                }
            }
            Op::Update => {
                let (main, _) = Self::main(&mut api).await?;
                let rebased = api
                    .control
                    .rebase(RebaseRequest {
                        repo: REPO.into(),
                        head: self.head.as_bytes().to_vec(),
                        onto: main.as_bytes().to_vec(),
                    })
                    .await?
                    .into_inner();
                self.head = rebased.head.as_deref().map_or(main, id);
            }
            Op::Merge { theirs } => {
                self.made += 1;
                let (name, email, secs) = author(self);
                let merged = api
                    .control
                    .merge(MergeRequest {
                        repo: REPO.into(),
                        ours: self.head.as_bytes().to_vec(),
                        theirs: theirs.as_bytes().to_vec(),
                        author_name: name,
                        author_email: email,
                        author_unix_secs: secs,
                        author_tz_offset_minutes: 0,
                        message: format!("c{c}-{} merge", self.made),
                    })
                    .await?
                    .into_inner();
                if let Some(merged) = merged.merged {
                    self.head = id(&merged);
                }
            }
            Op::Revert => {
                self.made += 1;
                let (name, email, secs) = author(self);
                let reverted = api
                    .control
                    .revert(RevertRequest {
                        repo: REPO.into(),
                        head: self.head.as_bytes().to_vec(),
                        changeset: self.head.as_bytes().to_vec(),
                        author_name: name,
                        author_email: email,
                        author_unix_secs: secs,
                        author_tz_offset_minutes: 0,
                        message: format!("c{c}-{} revert", self.made),
                    })
                    .await?
                    .into_inner();
                if let Some(reverted) = reverted.reverted {
                    self.head = id(&reverted);
                }
            }
            Op::Race => {
                let (main, version) = Self::main(&mut api).await?;
                let path = format!("race/c{c}-{}", self.made + 1);
                let target = self.create(&mut api, main, &path, "race").await?;
                api.control
                    .move_ref(MoveRefRequest {
                        repo: REPO.into(),
                        name: "main".into(),
                        target: target.as_bytes().to_vec(),
                        expected_version: Some(version),
                    })
                    .await?;
            }
        }
        Ok(None)
    }
}

/// Run the simulation of `seed`; the history on success, what went wrong
/// otherwise.
pub fn run(seed: u64) -> Result<History, String> {
    let rng = Arc::new(Rng::new(seed));
    let store = Arc::new(SimStore::new(rng.clone()));
    let key = TokenKey::from_hex(&"51".repeat(32)).expect("a valid key");
    let token = key
        .issue(PRINCIPAL, SystemTime::now() + Duration::from_secs(3600))
        .expect("a valid principal");
    let auth = Auth(format!("Bearer {token}").parse().expect("a token is ASCII"));

    let mut server = Server::start(&store, &key, &auth);
    let root = server.rt.block_on(async {
        let admin = Grant {
            principal: PRINCIPAL.into(),
            path_prefix: String::new(),
            role: Role::Admin,
        };
        store
            .inner
            .put_grant(SERVER_GRANTS, &admin)
            .await
            .map_err(|e| e.to_string())?;
        let init = server
            .api
            .clone()
            .control
            .init_repo(InitRepoRequest { repo: REPO.into() })
            .await
            .map_err(|st| st.to_string())?;
        Ok::<_, String>(id(&init.into_inner().main))
    })?;

    let mut clients: Vec<Client> = (0..CLIENTS)
        .map(|index| Client {
            index,
            head: root,
            made: 0,
            landing: None,
        })
        .collect();
    let mut submitted = BTreeSet::new();
    let mut mains = Vec::new();
    for step in 0..STEPS {
        if rng.below(25) == 0 {
            drop(server);
            server = Server::start(&store, &key, &auth);
        }
        if rng.below(8) == 0 {
            store.arm(Fault::MoveMain);
        }
        if rng.below(12) == 0 {
            store.arm(Fault::Finish);
        }
        submitted.extend(act(&rng, &server, &mut clients, step)?);
        let main = server
            .rt
            .block_on(store.inner.get_ref(REPO, "main"))
            .map_err(|e| e.to_string())?
            .ok_or("main is gone")?;
        mains.push(main.target);
    }

    // A last restart with nothing armed: every open request must finish.
    store.disarm();
    drop(server);
    let server = Server::start(&store, &key, &auth);
    finish(&server.rt, &store)?;
    server.rt.block_on(check(&store, &submitted, mains))
}

/// One step: some clients act at once, then the land workers run for a
/// while. Returns the requests submitted.
fn act(rng: &Rng, server: &Server, clients: &mut [Client], step: u32) -> Result<Vec<u64>, String> {
    let heads: Vec<ChangesetId> = clients.iter().map(|c| c.head).collect();
    let acting = 1 + rng.below(3);
    let mut ops = BTreeMap::new();
    for _ in 0..acting {
        let c = usize::try_from(rng.below(CLIENTS as u64)).expect("small");
        let op = choose(rng, &clients[c], &heads);
        ops.insert(c, op);
    }
    let work = clients
        .iter_mut()
        .filter_map(|client| ops.get(&client.index).map(|op| (client, *op)))
        .map(|(client, op)| {
            let api = server.api.clone();
            async move {
                let what = format!("step {step}: c{} {op:?}", client.index);
                client
                    .act(api, op)
                    .await
                    .or_else(|st| if expected(&st) { Ok(None) } else { Err(st) })
                    .map_err(|st| format!("{what}: {st}"))
            }
        });
    let runs = rng.below(30);
    let results = server.rt.block_on(async {
        let results = join_all(work).await;
        for _ in 0..runs {
            tokio::task::yield_now().await;
        }
        results
    });
    results.into_iter().filter_map(Result::transpose).collect()
}

/// Let the land workers run until no request is open, or fail naming the
/// ones still open.
fn finish(rt: &Runtime, store: &SimStore) -> Result<(), String> {
    for _ in 0..100 {
        let open = rt.block_on(async {
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            open_requests(&store.inner).await
        })?;
        if open.is_empty() {
            break;
        }
    }
    let open = rt.block_on(open_requests(&store.inner))?;
    if open.is_empty() {
        Ok(())
    } else {
        Err(format!("requests {open:?} never finished"))
    }
}

/// What `client` does next.
fn choose(rng: &Rng, client: &Client, heads: &[ChangesetId]) -> Op {
    if client.landing.is_some() {
        return if rng.below(4) == 0 {
            Op::Race
        } else {
            Op::Poll
        };
    }
    match rng.below(20) {
        0..=6 => Op::Commit {
            shared: rng.below(3) == 0,
            file: rng.below(3),
        },
        7..=11 => Op::Land,
        12..=14 => Op::Update,
        15 => Op::Merge {
            theirs: heads[usize::try_from(rng.below(heads.len() as u64)).expect("small")],
        },
        16..=17 => Op::Revert,
        _ => Op::Race,
    }
}

async fn open_requests(meta: &dyn MetadataStore) -> Result<Vec<u64>, String> {
    let all = meta
        .list_change_requests(REPO, None, u32::MAX)
        .await
        .map_err(|e| e.to_string())?;
    Ok(all
        .iter()
        .filter(|cr| {
            matches!(
                cr.status,
                ChangeRequestStatus::Pending | ChangeRequestStatus::Landing
            )
        })
        .map(|cr| cr.id)
        .collect())
}

/// Check the end state against the invariants; the history if they hold.
async fn check(
    store: &SimStore,
    submitted: &BTreeSet<u64>,
    mains: Vec<ChangesetId>,
) -> Result<History, String> {
    let meta = &store.inner;
    let err = |e: scion_store::StoreError| e.to_string();
    let read = async |id: ChangesetId| {
        meta.get_changeset(REPO, id)
            .await
            .map_err(err)?
            .ok_or_else(|| format!("changeset {id} is missing"))
    };

    let line = main_line(meta).await?;

    // `main` only moved forward along that line.
    let depth: BTreeMap<ChangesetId, usize> =
        line.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let mut last = usize::MAX;
    for (step, m) in mains.iter().enumerate() {
        match depth.get(m) {
            Some(d) if *d <= last => last = *d,
            Some(_) => return Err(format!("main moved back to {m} after step {step}")),
            None => return Err(format!("main was at {m} after step {step}, off its line")),
        }
    }

    // Every request finished; a landed one is on main exactly once, as a
    // copy of a changeset of its own stack (the stack's top may replay as
    // nothing, as a merge already on main does).
    let all = meta
        .list_change_requests(REPO, None, u32::MAX)
        .await
        .map_err(err)?;
    let listed: BTreeSet<u64> = all.iter().map(|cr| cr.id).collect();
    if listed != *submitted {
        return Err(format!(
            "submitted requests {submitted:?}, but the server lists {listed:?}"
        ));
    }
    let mut requests = Vec::new();
    for cr in all.iter().rev() {
        if cr.status == ChangeRequestStatus::Landed {
            let landed = cr
                .landed
                .ok_or(format!("request {} landed nowhere", cr.id))?;
            // The stack: the head's first-parent line down to where it
            // meets main at or below the request's base.
            let base = *depth
                .get(&cr.base)
                .ok_or(format!("request {}'s base is off main's line", cr.id))?;
            let mut stack = BTreeSet::new();
            let mut at = Some(cr.head);
            while let Some(id) = at {
                if depth.get(&id).is_some_and(|d| *d >= base) {
                    break;
                }
                let cs = read(id).await?;
                at = cs.first_parent();
                stack.insert(cs.message);
            }
            if !depth.contains_key(&landed) || !stack.contains(&read(landed).await?.message) {
                return Err(format!(
                    "request {} landed as {landed}, which is not a change of its own on main",
                    cr.id
                ));
            }
        }
        requests.push((cr.id, cr.status, cr.landed));
    }

    written(store).await?;
    Ok(History {
        main: line,
        mains,
        requests,
    })
}

/// Every object, changeset and derived pointer still holds what was first
/// written under its id, and no write differed from it on the way.
async fn written(store: &SimStore) -> Result<(), String> {
    let meta = &store.inner;
    let err = |e: scion_store::StoreError| e.to_string();
    let written = std::mem::take(&mut *store.written.lock().expect("poisoned"));
    if let Some(key) = written.overwrites.first() {
        return Err(format!("{key} was written twice with different contents"));
    }
    for (id, cs) in &written.changesets {
        if meta.get_changeset(REPO, *id).await.map_err(err)?.as_ref() != Some(cs) {
            return Err(format!("changeset {id} changed"));
        }
    }
    for (id, bytes) in &written.blobs {
        if meta.get_blob(*id).await.map_err(err)?.as_ref() != Some::<&Bytes>(bytes) {
            return Err(format!("blob {id} changed"));
        }
    }
    for (id, tree) in &written.trees {
        if meta.get_tree(*id).await.map_err(err)?.as_ref() != Some(tree) {
            return Err(format!("tree {id} changed"));
        }
    }
    for ((repo, ty, version, cs), pointer) in &written.derived {
        let now = meta
            .get_derived(repo, ty, *version, *cs)
            .await
            .map_err(err)?;
        if now.as_ref() != Some(pointer) {
            return Err(format!("derived {ty} v{version} of {cs} changed"));
        }
    }
    Ok(())
}

/// `main`'s first-parent line, newest first, each message on it once.
async fn main_line(meta: &dyn MetadataStore) -> Result<Vec<ChangesetId>, String> {
    let err = |e: scion_store::StoreError| e.to_string();
    let mut at = meta
        .get_ref(REPO, "main")
        .await
        .map_err(err)?
        .map(|main| main.target);
    let mut line = Vec::new();
    let mut messages = BTreeSet::new();
    while let Some(id) = at {
        let cs = meta
            .get_changeset(REPO, id)
            .await
            .map_err(err)?
            .ok_or_else(|| format!("changeset {id} is missing"))?;
        if !messages.insert(cs.message.clone()) {
            return Err(format!("{:?} is on main more than once", cs.message));
        }
        line.push(id);
        at = cs.first_parent();
    }
    Ok(line)
}
