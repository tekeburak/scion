//! The serial land worker. Each repository with change requests gets one
//! task, started by its first submit (or at server start if requests are
//! waiting) and woken by every later one. It takes pending requests in
//! submission order, rebases each head onto `main` through [`rebase`](crate::control::rebase) and
//! moves `main` with one compare-and-swap; while that CAS is in flight, the
//! next request is claimed and rebased onto the `main` the CAS makes, so
//! each land costs one rebase rather than a rebase plus the CAS and its
//! bookkeeping. Submitting returns at once, so landers never queue behind
//! each other's round trips; the worker never waits on a client. A client
//! that asks to wait is told when its request is finished, without polling.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use scion_proto::trace::timed;
use scion_store::{
    ChangeRequest, ChangeRequestStatus, LandOutcome, MetadataStore, ObjectStore, RefState, Role,
    StoreError,
};
use scion_types::{Changeset, ChangesetId};
use tokio::sync::{Notify, broadcast};
use tonic::{Request, Status};

use crate::auth::{Scope, authorize, principal};
use crate::control::{MAX_LOG_LIMIT, Rebased, replay, spawn_history};
use crate::store_status;

/// Longest a waiting `SubmitChangeRequest` or `GetChangeRequest` holds its
/// reply; below the client's 30-second deadline, so a long queue answers
/// with the request still open and the client asks again.
const MAX_WAIT: Duration = Duration::from_secs(20);

/// The land workers of every repository this server has seen requests for.
pub(crate) struct LandWorkers {
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    workers: Mutex<HashMap<String, Arc<Worker>>>,
}

/// Finished requests a waiting client may fall behind by before it reads
/// its own from the store instead.
const FINISHED_QUEUED: usize = 64;

/// One repository's worker: woken by submits, sending every finished
/// request to the clients waiting on one, so none of them reads it again.
struct Worker {
    wake: Notify,
    finished: broadcast::Sender<ChangeRequest>,
}

impl LandWorkers {
    pub(crate) fn new(meta: Arc<dyn MetadataStore>, objects: Arc<dyn ObjectStore>) -> Self {
        Self {
            meta,
            objects,
            workers: Mutex::new(HashMap::new()),
        }
    }

    /// `repo`'s worker, started if it is not running.
    fn worker(&self, repo: &str) -> Arc<Worker> {
        let mut workers = self.workers.lock().expect("poisoned");
        workers
            .entry(repo.to_owned())
            .or_insert_with(|| {
                let worker = Arc::new(Worker {
                    wake: Notify::new(),
                    finished: broadcast::channel(FINISHED_QUEUED).0,
                });
                tokio::spawn(work(
                    self.meta.clone(),
                    self.objects.clone(),
                    repo.to_owned(),
                    worker.clone(),
                ));
                worker
            })
            .clone()
    }

    /// Wake `repo`'s worker, starting it first if it is not running. A wake
    /// while the worker is busy is kept, so the request it announces is
    /// claimed before the worker sleeps again.
    pub(crate) fn wake(&self, repo: &str) {
        self.worker(repo).wake.notify_one();
    }

    /// Request `id` of `repo` once it is finished, or as it is after
    /// [`MAX_WAIT`]; `None` if there is no such request.
    pub(crate) async fn wait(
        &self,
        repo: &str,
        id: u64,
    ) -> Result<Option<ChangeRequest>, StoreError> {
        let deadline = tokio::time::Instant::now() + MAX_WAIT;
        let cr = self.meta.get_change_request(repo, id).await?;
        if !cr.as_ref().is_some_and(is_open) {
            return Ok(cr);
        }
        // Only an open request, so of a repository that exists, reaches its
        // worker.
        let mut finished = self.worker(repo).finished.subscribe();
        loop {
            // Listening before reading, so a finish between the read and
            // the wait is still received.
            let cr = self.meta.get_change_request(repo, id).await?;
            if !cr.as_ref().is_some_and(is_open) {
                return Ok(cr);
            }
            loop {
                match tokio::time::timeout_at(deadline, finished.recv()).await {
                    Err(_) => return Ok(cr),
                    Ok(Ok(done)) if done.id == id => return Ok(Some(done)),
                    Ok(Ok(_)) => {}
                    // Fell behind: read it again.
                    Ok(Err(_)) => break,
                }
            }
        }
    }
}

/// Reject `cr` of `repo` for `request`'s principal, its submitter or an
/// admin of the repository. One that is no longer `pending` or
/// `needs_rebase` is refused, and stays as it is.
pub(crate) async fn reject<T>(
    meta: &dyn MetadataStore,
    request: &Request<T>,
    repo: &str,
    cr: &ChangeRequest,
) -> Result<ChangeRequest, Status> {
    if principal(request)? != cr.submitter {
        authorize(meta, request, repo, Role::Admin).await?;
    }
    let cr = meta
        .reject_change_request(repo, cr.id)
        .await
        .map_err(store_status)?;
    if cr.status != ChangeRequestStatus::Rejected {
        return Err(Status::failed_precondition(format!(
            "change request {} is {}; only one that is pending or needs a rebase can be rejected",
            cr.id, cr.status
        )));
    }
    Ok(cr)
}

fn is_open(cr: &ChangeRequest) -> bool {
    matches!(
        cr.status,
        ChangeRequestStatus::Pending | ChangeRequestStatus::Landing
    )
}

/// `repo`'s worker: land every pending request, then sleep until woken.
/// Its first run, and the run after any failure, first finishes the request
/// a stopped or failed run left `landing`.
async fn work(
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    repo: String,
    worker: Arc<Worker>,
) {
    let mut resume = true;
    loop {
        worker.wake.notified().await;
        resume = match land_pending(&meta, &objects, &repo, resume, &worker.finished).await {
            Ok(()) => false,
            Err(e) => {
                eprintln!("scion-server: land worker for {repo:?}: {e}");
                true
            }
        };
    }
}

async fn land_pending(
    meta: &Arc<dyn MetadataStore>,
    objects: &Arc<dyn ObjectStore>,
    repo: &str,
    resume: bool,
    finished: &broadcast::Sender<ChangeRequest>,
) -> Result<(), Status> {
    if resume {
        while let Some(cr) = meta
            .landing_change_request(repo)
            .await
            .map_err(store_status)?
        {
            let outcome = land(&**meta, &**objects, repo, &cr, true).await?;
            finish(meta, objects, repo, &cr, outcome, finished).await?;
        }
    }
    // The next request, when it was claimed while the previous one's CAS
    // was in flight, and its step, prepared onto the `main` that CAS made.
    let (store, blobs) = (&**meta, &**objects);
    let mut next = None;
    let mut ready = None;
    loop {
        if next.is_none() {
            next = timed("claim", meta.claim_next_change_request(repo))
                .await
                .map_err(store_status)?;
        }
        let Some(cr) = next.take() else {
            return Ok(());
        };
        let mut step = match ready.take() {
            Some(step) => step,
            None => prepare(store, blobs, repo, &cr, cr.head, main(store, repo).await?).await?,
        };
        loop {
            let (from, to) = match step {
                Step::Done(outcome) => {
                    finish(meta, objects, repo, &cr, outcome, finished).await?;
                    break;
                }
                Step::Move { from, to } => (from, to),
            };
            // While this CAS is in flight, the next request is claimed and
            // rebased onto the `main` the CAS makes.
            let made = RefState {
                target: to,
                version: from.version + 1,
            };
            let this = async {
                match timed("cas", meta.move_ref(repo, "main", from.version, to)).await {
                    Ok(_) => {
                        let landed = LandOutcome::Landed(to);
                        finish(meta, objects, repo, &cr, landed, finished).await?;
                        Ok(true)
                    }
                    Err(StoreError::CasMismatch { .. }) => Ok(false),
                    Err(e) => Err(store_status(e)),
                }
            };
            let ahead = async {
                if next.is_none() {
                    next = timed("claim", meta.claim_next_change_request(repo))
                        .await
                        .map_err(store_status)?;
                }
                match &next {
                    Some(n) => prepare(store, blobs, repo, n, n.head, made).await.map(Some),
                    None => Ok(None),
                }
            };
            let (landed, ahead) = tokio::join!(this, ahead);
            let (landed, ahead) = (landed?, ahead?);
            if landed {
                ready = ahead;
                break;
            }
            // Another writer (`MoveRef`, an import) moved `main` meanwhile:
            // the rebased stack is replayed onto the new `main`, and what was
            // prepared for the next request, which assumed this CAS, is
            // dropped. Each retry follows a move of `main`, never a timer.
            step = prepare(store, blobs, repo, &cr, to, main(store, repo).await?).await?;
        }
    }
}

/// Record `cr`'s `outcome` and send it to the clients waiting on it.
/// Nothing a land waits on derived the path history of what landed, so a
/// task stores it now.
async fn finish(
    meta: &Arc<dyn MetadataStore>,
    objects: &Arc<dyn ObjectStore>,
    repo: &str,
    cr: &ChangeRequest,
    outcome: LandOutcome,
    finished: &broadcast::Sender<ChangeRequest>,
) -> Result<(), Status> {
    let landed = match outcome {
        LandOutcome::Landed(id) => Some(id),
        _ => None,
    };
    let cr = timed("finish", meta.finish_change_request(repo, cr.id, outcome))
        .await
        .map_err(store_status)?;
    // No receiver means no client is waiting.
    let _ = finished.send(cr);
    if let Some(id) = landed {
        spawn_history(meta.clone(), objects.clone(), repo.to_owned(), id);
    }
    Ok(())
}

async fn main(meta: &dyn MetadataStore, repo: &str) -> Result<RefState, Status> {
    timed("main", meta.get_ref(repo, "main"))
        .await
        .map_err(store_status)?
        .ok_or_else(|| Status::not_found(format!("ref \"main\" in {repo:?}")))
}

/// What landing a request onto one `main` comes to.
enum Step {
    /// Finished without moving `main`.
    Done(LandOutcome),
    /// Lands by moving `main` from `from` to `to`.
    Move { from: RefState, to: ChangesetId },
}

/// Rebase `head`, `cr`'s head or its rebased stack, onto `main`, unless it
/// changes a path `cr`'s submitter may not change. The submitter's grants
/// are read now, so one revoked since the submit counts, and changesets
/// someone else created are held to the submitter's grants.
async fn prepare(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    repo: &str,
    cr: &ChangeRequest,
    head: ChangesetId,
    main: RefState,
) -> Result<Step, Status> {
    let scope = timed("scope", Scope::of(meta, repo, &cr.submitter, Role::Writer)).await?;
    // The path history of what it creates is stored once it has landed.
    let rebased = replay(meta, objects, repo, (head, main.target), &scope, false).await?;
    Ok(match rebased {
        Rebased::Denied { paths } => Step::Done(LandOutcome::Denied(
            paths.iter().map(|p| p.as_str().to_owned()).collect(),
        )),
        Rebased::Conflict { paths, .. } => Step::Done(LandOutcome::NeedsRebase(
            paths.iter().map(|p| p.as_str().to_owned()).collect(),
        )),
        Rebased::Clean { head, .. } if head == main.target => Step::Done(LandOutcome::Rejected),
        Rebased::Clean { head, .. } => Step::Move {
            from: main,
            to: head,
        },
    })
}

/// Land `cr` by itself: [`prepare`] it onto `main` and move `main` by CAS,
/// replaying onto the new `main` after a lost CAS.
///
/// Landing again is idempotent: rebased changesets are content-addressed,
/// so a request stopped before its CAS recreates the same ids. One
/// `resumed` after its CAS finds its head, or the rebased copy of it, on
/// `main`'s first-parent line above the request's base, where `main` may
/// have moved on since; it landed there, and is not replayed again (a
/// stack whose replay is no no-op, such as a change and its revert, would
/// land twice).
async fn land(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    repo: &str,
    cr: &ChangeRequest,
    resumed: bool,
) -> Result<LandOutcome, Status> {
    if resumed {
        if let Some(copy) = landed_copy(meta, repo, cr).await? {
            return Ok(LandOutcome::Landed(copy));
        }
    }
    let mut head = cr.head;
    loop {
        match prepare(meta, objects, repo, cr, head, main(meta, repo).await?).await? {
            Step::Done(outcome) => return Ok(outcome),
            Step::Move { from, to } => {
                match timed("cas", meta.move_ref(repo, "main", from.version, to)).await {
                    Ok(_) => return Ok(LandOutcome::Landed(to)),
                    Err(StoreError::CasMismatch { .. }) => head = to,
                    Err(e) => return Err(store_status(e)),
                }
            }
        }
    }
}

/// `cr`'s head or its rebased copy, if a stopped run put it on `main`: the
/// walk down `main`'s first-parent line stops at `cr.base`'s generation, so
/// it reads only what landed since the request was submitted. A merge on
/// top of the stack whose other parents `main` has already replays as
/// nothing (see [`rebase`](crate::control::rebase)), so the land's tip is the copy of the merge's
/// first parent: that copy counts too, down a run of merges. Only a
/// rebased copy of it: a replay below the head always changes its parent,
/// so the first parent itself on `main` is history the request was built
/// on, not its land.
async fn landed_copy(
    meta: &dyn MetadataStore,
    repo: &str,
    cr: &ChangeRequest,
) -> Result<Option<ChangesetId>, Status> {
    let generation = async |id| {
        meta.generation(repo, id)
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::data_loss(format!("changeset {id} is missing")))
    };
    let floor = generation(cr.base).await?;
    let mut tips = vec![cr.head];
    let mut top = cr.head;
    // No run of merges is longer than a stack `rebase` replays.
    while tips.len() <= MAX_LOG_LIMIT as usize {
        let cs = meta
            .get_changeset(repo, top)
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::data_loss(format!("changeset {top} is missing")))?;
        match cs.parents[..] {
            [first, _, ..] => {
                tips.push(first);
                top = first;
            }
            _ => break,
        }
    }
    let mut at = meta
        .get_ref(repo, "main")
        .await
        .map_err(store_status)?
        .map(|main| main.target);
    while let Some(id) = at {
        if generation(id).await? <= floor {
            break;
        }
        for (i, &tip) in tips.iter().enumerate() {
            if (i == 0 || tip != id) && is_copy(meta, repo, tip, id).await? {
                return Ok(Some(id));
            }
        }
        at = meta
            .get_changeset(repo, id)
            .await
            .map_err(store_status)?
            .and_then(|cs| cs.first_parent());
    }
    Ok(None)
}

/// Whether `landed` is `head` or `head` replayed by [`rebase`](crate::control::rebase), which keeps
/// everything but the parents and the file changes.
async fn is_copy(
    meta: &dyn MetadataStore,
    repo: &str,
    head: ChangesetId,
    landed: ChangesetId,
) -> Result<bool, Status> {
    if head == landed {
        return Ok(true);
    }
    let read = async |id| {
        meta.get_changeset(repo, id)
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::data_loss(format!("changeset {id} is missing")))
    };
    let (head, landed) = (read(head).await?, read(landed).await?);
    Ok(Changeset {
        parents: landed.parents.clone(),
        file_changes: landed.file_changes.clone(),
        ..head
    } == landed)
}
