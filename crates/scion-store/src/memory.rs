//! In-memory store, for tests and the simulator.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::RwLock;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use scion_types::{BlobId, Changeset, ChangesetId, Tree, TreeId};

use crate::{
    ChangeRequest, ChangeRequestStatus, DerivedPointer, LandOutcome, MetadataStore, ObjectStore,
    RefState, Result, StoreError, is_allowed_ref_name,
};

#[derive(Debug, Default)]
struct Metadata {
    repos: HashSet<String>,
    changesets: HashMap<(String, ChangesetId), (Changeset, u64)>,
    refs: HashMap<String, BTreeMap<String, RefState>>,
    derived: HashMap<(String, String, u32, ChangesetId), Vec<u8>>,
    checkpoints: HashMap<String, BTreeMap<Vec<u8>, ChangesetId>>,
    /// Per repository, by id; ids are shared by all repositories, like a
    /// `BIGSERIAL`.
    change_requests: HashMap<String, BTreeMap<u64, ChangeRequest>>,
    last_request_id: u64,
}

#[derive(Debug, Default)]
struct Objects {
    blobs: HashMap<BlobId, Bytes>,
    trees: HashMap<TreeId, Tree>,
}

/// An in-memory [`MetadataStore`] + [`ObjectStore`].
#[derive(Debug, Default)]
pub struct MemoryStore {
    meta: RwLock<Metadata>,
    objects: RwLock<Objects>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A store holding only repository `repo`, with nothing in it.
    #[must_use]
    pub fn with_repo(repo: &str) -> Self {
        let store = Self::default();
        store
            .meta
            .write()
            .expect("poisoned")
            .repos
            .insert(repo.to_owned());
        store
    }
}

impl Metadata {
    fn check_repo(&self, repo: &str) -> Result<()> {
        if self.repos.contains(repo) {
            Ok(())
        } else {
            Err(StoreError::RepoNotFound(repo.to_owned()))
        }
    }
}

#[async_trait]
impl MetadataStore for MemoryStore {
    async fn create_repo(&self, name: &str) -> Result<()> {
        let mut m = self.meta.write().expect("poisoned");
        m.repos.insert(name.to_owned());
        Ok(())
    }

    async fn put_changesets(&self, batch: &[Changeset]) -> Result<()> {
        let mut m = self.meta.write().expect("poisoned");
        for cs in batch {
            m.check_repo(&cs.repo)?;
        }
        for cs in batch {
            let id = cs.id();
            let key = (cs.repo.clone(), id);
            if m.changesets.contains_key(&key) {
                continue;
            }
            let mut generation = 0;
            for p in &cs.parents {
                match m.changesets.get(&(cs.repo.clone(), *p)) {
                    Some((_, g)) => generation = generation.max(g + 1),
                    None => return Err(StoreError::UnknownParent(id, *p)),
                }
            }
            m.changesets.insert(key, (cs.clone(), generation));
        }
        Ok(())
    }

    async fn get_changeset(&self, repo: &str, id: ChangesetId) -> Result<Option<Changeset>> {
        let m = self.meta.read().expect("poisoned");
        Ok(m.changesets
            .get(&(repo.to_owned(), id))
            .map(|(cs, _)| cs.clone()))
    }

    async fn generation(&self, repo: &str, id: ChangesetId) -> Result<Option<u64>> {
        let m = self.meta.read().expect("poisoned");
        Ok(m.changesets.get(&(repo.to_owned(), id)).map(|(_, g)| *g))
    }

    async fn create_ref(&self, repo: &str, name: &str, target: ChangesetId) -> Result<RefState> {
        if !is_allowed_ref_name(name) {
            return Err(StoreError::RefNameNotAllowed(name.to_owned()));
        }
        let mut m = self.meta.write().expect("poisoned");
        m.check_repo(repo)?;
        let refs = m.refs.entry(repo.to_owned()).or_default();
        if refs.contains_key(name) {
            return Err(StoreError::RefExists(name.to_owned()));
        }
        let state = RefState { target, version: 0 };
        refs.insert(name.to_owned(), state);
        Ok(state)
    }

    async fn get_ref(&self, repo: &str, name: &str) -> Result<Option<RefState>> {
        let m = self.meta.read().expect("poisoned");
        Ok(m.refs.get(repo).and_then(|r| r.get(name)).copied())
    }

    async fn move_ref(
        &self,
        repo: &str,
        name: &str,
        expected_version: u64,
        new_target: ChangesetId,
    ) -> Result<RefState> {
        let mut m = self.meta.write().expect("poisoned");
        m.check_repo(repo)?;
        let state = m
            .refs
            .get_mut(repo)
            .and_then(|r| r.get_mut(name))
            .ok_or_else(|| StoreError::RefNotFound(name.to_owned()))?;
        if state.version != expected_version {
            return Err(StoreError::CasMismatch {
                expected: expected_version,
                actual: state.version,
            });
        }
        state.target = new_target;
        state.version += 1;
        Ok(*state)
    }

    async fn list_refs(&self, repo: &str) -> Result<Vec<(String, RefState)>> {
        let m = self.meta.read().expect("poisoned");
        Ok(m.refs
            .get(repo)
            .map(|r| r.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default())
    }

    async fn put_derived_many(&self, repo: &str, pointers: Vec<DerivedPointer>) -> Result<()> {
        let mut m = self.meta.write().expect("poisoned");
        m.check_repo(repo)?;
        for (ty, version, cs, pointer) in pointers {
            m.derived
                .insert((repo.to_owned(), ty, version, cs), pointer);
        }
        Ok(())
    }

    async fn get_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
    ) -> Result<Option<Vec<u8>>> {
        let m = self.meta.read().expect("poisoned");
        Ok(m.derived
            .get(&(repo.to_owned(), ty.to_owned(), version, cs))
            .cloned())
    }

    async fn put_ingest_checkpoints(
        &self,
        repo: &str,
        done: &[(Vec<u8>, ChangesetId)],
    ) -> Result<()> {
        let mut m = self.meta.write().expect("poisoned");
        m.check_repo(repo)?;
        let checkpoints = m.checkpoints.entry(repo.to_owned()).or_default();
        for (sha, cs) in done {
            checkpoints.entry(sha.clone()).or_insert(*cs);
        }
        Ok(())
    }

    async fn ingest_checkpoints(&self, repo: &str) -> Result<Vec<(Vec<u8>, ChangesetId)>> {
        let m = self.meta.read().expect("poisoned");
        Ok(m.checkpoints
            .get(repo)
            .map(|c| c.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default())
    }

    async fn submit_change_request(
        &self,
        repo: &str,
        head: ChangesetId,
        base: ChangesetId,
    ) -> Result<ChangeRequest> {
        let mut m = self.meta.write().expect("poisoned");
        m.check_repo(repo)?;
        m.last_request_id += 1;
        let request = ChangeRequest {
            id: m.last_request_id,
            head,
            base,
            status: ChangeRequestStatus::Pending,
            landed: None,
            conflicts: Vec::new(),
            submitted_at: SystemTime::now(),
            landed_at: None,
        };
        m.change_requests
            .entry(repo.to_owned())
            .or_default()
            .insert(request.id, request.clone());
        Ok(request)
    }

    async fn get_change_request(&self, repo: &str, id: u64) -> Result<Option<ChangeRequest>> {
        let m = self.meta.read().expect("poisoned");
        Ok(m.change_requests
            .get(repo)
            .and_then(|r| r.get(&id))
            .cloned())
    }

    async fn list_change_requests(
        &self,
        repo: &str,
        status: Option<ChangeRequestStatus>,
        limit: u32,
    ) -> Result<Vec<ChangeRequest>> {
        let m = self.meta.read().expect("poisoned");
        m.check_repo(repo)?;
        Ok(m.change_requests
            .get(repo)
            .map(|r| {
                r.values()
                    .rev()
                    .filter(|cr| status.is_none_or(|st| cr.status == st))
                    .take(limit as usize)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn find_landings(
        &self,
        repo: &str,
        changesets: &[ChangesetId],
    ) -> Result<Vec<(ChangesetId, ChangeRequest)>> {
        let m = self.meta.read().expect("poisoned");
        m.check_repo(repo)?;
        let Some(requests) = m.change_requests.get(repo) else {
            return Ok(Vec::new());
        };
        Ok(changesets
            .iter()
            .filter_map(|cs| {
                requests
                    .values()
                    .find(|cr| {
                        cr.status == ChangeRequestStatus::Landed
                            && (cr.head == *cs || cr.landed == Some(*cs))
                    })
                    .map(|cr| (*cs, cr.clone()))
            })
            .collect())
    }

    async fn claim_next_change_request(&self, repo: &str) -> Result<Option<ChangeRequest>> {
        let mut m = self.meta.write().expect("poisoned");
        let next = m.change_requests.get_mut(repo).and_then(|r| {
            r.values_mut()
                .find(|cr| cr.status == ChangeRequestStatus::Pending)
        });
        Ok(next.map(|cr| {
            cr.status = ChangeRequestStatus::Landing;
            cr.clone()
        }))
    }

    async fn landing_change_request(&self, repo: &str) -> Result<Option<ChangeRequest>> {
        let m = self.meta.read().expect("poisoned");
        Ok(m.change_requests.get(repo).and_then(|r| {
            r.values()
                .find(|cr| cr.status == ChangeRequestStatus::Landing)
                .cloned()
        }))
    }

    async fn finish_change_request(
        &self,
        repo: &str,
        id: u64,
        outcome: LandOutcome,
    ) -> Result<ChangeRequest> {
        let mut m = self.meta.write().expect("poisoned");
        let cr = m
            .change_requests
            .get_mut(repo)
            .and_then(|r| r.get_mut(&id))
            .ok_or(StoreError::ChangeRequestNotFound(id))?;
        if cr.status != ChangeRequestStatus::Landing {
            return Err(StoreError::ChangeRequestNotLanding {
                id,
                status: cr.status,
            });
        }
        cr.status = outcome.status();
        match outcome {
            LandOutcome::Landed(cs) => {
                cr.landed = Some(cs);
                cr.landed_at = Some(SystemTime::now());
            }
            LandOutcome::NeedsRebase(paths) => cr.conflicts = paths,
            LandOutcome::Rejected => {}
        }
        Ok(cr.clone())
    }

    async fn repos_with_open_change_requests(&self) -> Result<Vec<String>> {
        let m = self.meta.read().expect("poisoned");
        let mut repos: Vec<String> = m
            .change_requests
            .iter()
            .filter(|(_, requests)| {
                requests.values().any(|cr| {
                    matches!(
                        cr.status,
                        ChangeRequestStatus::Pending | ChangeRequestStatus::Landing
                    )
                })
            })
            .map(|(repo, _)| repo.clone())
            .collect();
        repos.sort();
        Ok(repos)
    }
}

#[async_trait]
impl ObjectStore for MemoryStore {
    async fn put_blob(&self, bytes: Bytes) -> Result<BlobId> {
        let id = BlobId::hash(&bytes);
        self.objects
            .write()
            .expect("poisoned")
            .blobs
            .entry(id)
            .or_insert(bytes);
        Ok(id)
    }

    async fn get_blob(&self, id: BlobId) -> Result<Option<Bytes>> {
        Ok(self
            .objects
            .read()
            .expect("poisoned")
            .blobs
            .get(&id)
            .cloned())
    }

    async fn has_blobs(&self, ids: &[BlobId]) -> Result<Vec<bool>> {
        let o = self.objects.read().expect("poisoned");
        Ok(ids.iter().map(|id| o.blobs.contains_key(id)).collect())
    }

    async fn put_tree(&self, tree: &Tree) -> Result<TreeId> {
        let id = tree.id();
        self.objects
            .write()
            .expect("poisoned")
            .trees
            .entry(id)
            .or_insert_with(|| tree.clone());
        Ok(id)
    }

    async fn get_tree(&self, id: TreeId) -> Result<Option<Tree>> {
        Ok(self
            .objects
            .read()
            .expect("poisoned")
            .trees
            .get(&id)
            .cloned())
    }
}

crate::conformance_tests!(memory_conformance, async { MemoryStore::new() });
