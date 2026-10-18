//! The serial land worker. Each repository with change requests gets one
//! task, started by its first submit (or at server start if requests are
//! waiting) and woken by every later one. It takes pending requests in
//! submission order, rebases each head onto `main` through [`rebase`] and
//! moves `main` with one compare-and-swap. Submitting returns at once, so
//! landers never queue behind each other's round trips; the worker never
//! waits on a client.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use scion_store::{LandOutcome, MetadataStore, ObjectStore, StoreError};
use scion_types::{Changeset, ChangesetId};
use tokio::sync::Notify;
use tonic::Status;

use crate::control::{Rebased, rebase};
use crate::store_status;

/// The land workers of every repository this server has seen requests for.
pub(crate) struct LandWorkers {
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    wakers: Mutex<HashMap<String, Arc<Notify>>>,
}

impl LandWorkers {
    pub(crate) fn new(meta: Arc<dyn MetadataStore>, objects: Arc<dyn ObjectStore>) -> Self {
        Self {
            meta,
            objects,
            wakers: Mutex::new(HashMap::new()),
        }
    }

    /// Wake `repo`'s worker, starting it first if it is not running. A wake
    /// while the worker is busy is kept, so the request it announces is
    /// claimed before the worker sleeps again.
    pub(crate) fn wake(&self, repo: &str) {
        let mut wakers = self.wakers.lock().expect("poisoned");
        let notify = wakers.entry(repo.to_owned()).or_insert_with(|| {
            let notify = Arc::new(Notify::new());
            tokio::spawn(work(
                self.meta.clone(),
                self.objects.clone(),
                repo.to_owned(),
                notify.clone(),
            ));
            notify
        });
        notify.notify_one();
    }
}

/// `repo`'s worker: land every pending request, then sleep until woken.
/// Its first run, and the run after any failure, first finishes the request
/// a stopped or failed run left `landing`.
async fn work(
    meta: Arc<dyn MetadataStore>,
    objects: Arc<dyn ObjectStore>,
    repo: String,
    notify: Arc<Notify>,
) {
    let mut resume = true;
    loop {
        notify.notified().await;
        resume = match land_pending(&*meta, &*objects, &repo, resume).await {
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
) -> Result<(), Status> {
    if resume {
        while let Some(cr) = meta
            .landing_change_request(repo)
            .await
            .map_err(store_status)?
        {
            let outcome = land(meta, objects, repo, cr.head, true).await?;
            meta.finish_change_request(repo, cr.id, outcome)
                .await
                .map_err(store_status)?;
        }
    }
    while let Some(cr) = meta
        .claim_next_change_request(repo)
        .await
        .map_err(store_status)?
    {
        let outcome = land(meta, objects, repo, cr.head, false).await?;
        meta.finish_change_request(repo, cr.id, outcome)
            .await
            .map_err(store_status)?;
    }
    Ok(())
}

/// Rebase `head` onto `main` and move `main` to the result by CAS. A lost
/// CAS means another writer (`MoveRef`, an import) moved `main` meanwhile,
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
    mut head: ChangesetId,
    resumed: bool,
) -> Result<LandOutcome, Status> {
    loop {
        let main = meta
            .get_ref(repo, "main")
            .await
            .map_err(store_status)?
            .ok_or_else(|| Status::not_found(format!("ref \"main\" in {repo:?}")))?;
        let rebased = match rebase(meta, objects, repo, head, main.target).await? {
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
