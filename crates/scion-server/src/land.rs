//! The serial land worker. Each repository with change requests gets one
//! task, started by its first submit (or at server start if requests are
//! waiting) and woken by every later one. It takes pending requests in
//! submission order, rebases each head onto `main` through [`rebase`] and
//! moves `main` with one compare-and-swap. Submitting returns at once, so
//! landers never queue behind each other's round trips; the worker never
//! waits on a client. A client that asks to wait is told when its request
//! is finished, without polling.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use scion_store::{
    ChangeRequest, ChangeRequestStatus, LandOutcome, MetadataStore, ObjectStore, Role, StoreError,
};
use scion_types::{Changeset, ChangesetId};
use tokio::sync::Notify;
use tonic::Status;

use crate::auth::Scope;
use crate::control::{Rebased, rebase};
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

/// One repository's worker: woken by submits, announcing every finished
/// request to the clients waiting on one.
#[derive(Default)]
struct Worker {
    wake: Notify,
    finished: Notify,
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
                let worker = Arc::new(Worker::default());
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
        let worker = self.worker(repo);
        loop {
            // Listening before reading, so a finish between the read and
            // the sleep still wakes it.
            let finished = worker.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            let cr = self.meta.get_change_request(repo, id).await?;
            if !cr.as_ref().is_some_and(is_open)
                || tokio::time::timeout_at(deadline, finished).await.is_err()
            {
                return Ok(cr);
            }
        }
    }
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
        resume = match land_pending(&*meta, &*objects, &repo, resume, &worker.finished).await {
            Ok(()) => false,
            Err(e) => {
                eprintln!("scion-server: land worker for {repo:?}: {e}");
                true
            }
        };
    }
}

async fn land_pending(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    repo: &str,
    resume: bool,
    finished: &Notify,
) -> Result<(), Status> {
    if resume {
        while let Some(cr) = meta
            .landing_change_request(repo)
            .await
            .map_err(store_status)?
        {
            let outcome = land(meta, objects, repo, &cr, true).await?;
            meta.finish_change_request(repo, cr.id, outcome)
                .await
                .map_err(store_status)?;
            finished.notify_waiters();
        }
    }
    while let Some(cr) = meta
        .claim_next_change_request(repo)
        .await
        .map_err(store_status)?
    {
        let outcome = land(meta, objects, repo, &cr, false).await?;
        meta.finish_change_request(repo, cr.id, outcome)
            .await
            .map_err(store_status)?;
        finished.notify_waiters();
    }
    Ok(())
}

/// Rebase `cr`'s head onto `main` and move `main` to the result by CAS,
/// unless it changes a path its submitter may not change. The submitter's
/// grants are read now, so one revoked since the submit counts, and
/// changesets someone else created are held to the submitter's grants. A
/// lost CAS means another writer (`MoveRef`, an import) moved `main` meanwhile,
/// so the rebased stack is replayed onto the new `main`; each retry follows
/// a move of `main`, never a timer.
///
/// Landing again is idempotent: rebased changesets are content-addressed,
/// so a request stopped before its CAS recreates the same ids. One
/// `resumed` after its CAS finds `main` already holding the change; it
/// landed if `main` is its head or the rebased copy of it.
async fn land(
    meta: &dyn MetadataStore,
    objects: &dyn ObjectStore,
    repo: &str,
    cr: &ChangeRequest,
    resumed: bool,
) -> Result<LandOutcome, Status> {
    let scope = Scope::of(meta, repo, &cr.submitter, Role::Writer).await?;
    let mut head = cr.head;
    loop {
        let main = meta
            .get_ref(repo, "main")
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::not_found(format!("ref \"main\" in {repo:?}")))?;
        let rebased = match rebase(meta, objects, repo, head, main.target, &scope).await? {
            Rebased::Denied { paths } => {
                return Ok(LandOutcome::Denied(
                    paths.iter().map(|p| p.as_str().to_owned()).collect(),
                ));
            }
            Rebased::Conflict { paths, .. } => {
                return Ok(LandOutcome::NeedsRebase(
                    paths.iter().map(|p| p.as_str().to_owned()).collect(),
                ));
            }
            Rebased::Clean { head: new, .. } if new == main.target => {
                return Ok(
                    if resumed && is_copy(meta, repo, head, main.target).await? {
                        LandOutcome::Landed(main.target)
                    } else {
                        LandOutcome::Rejected
                    },
                );
            }
            Rebased::Clean { head, .. } => head,
        };
        match meta.move_ref(repo, "main", main.version, rebased).await {
            Ok(_) => return Ok(LandOutcome::Landed(rebased)),
            Err(StoreError::CasMismatch { .. }) => head = rebased,
            Err(e) => return Err(store_status(e)),
        }
    }
}

/// Whether `landed` is `head` or `head` replayed by [`rebase`], which keeps
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
