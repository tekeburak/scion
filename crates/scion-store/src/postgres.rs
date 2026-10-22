//! Postgres-backed [`MetadataStore`].
//!
//! Plain SQL over `sqlx` so that `CockroachDB` (Postgres wire protocol) remains a
//! drop-in target. The schema lives in `migrations/`; the canonical encoding of
//! every changeset is stored verbatim in `changesets.canonical`, so reads
//! reproduce the exact object that was hashed, and the structured columns
//! exist for querying.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use scion_types::{BlobId, Changeset, ChangesetId, Decode, Encode, FileChange, Tree, TreeId};
use sqlx::postgres::{PgConnectOptions, PgListener, PgPool, PgPoolOptions};
use sqlx::{Executor, Postgres};

use crate::recent::Recent;
use crate::{
    ChangeRequest, ChangeRequestStatus, DerivedPointer, Grant, LandOutcome, MetadataStore,
    RefState, Result, Role, SERVER_GRANTS, StoreError, is_allowed_ref_name,
};

/// Embedded migrations from the repository's `migrations/` directory.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Bytes of recently read or written changesets kept in memory.
const CHANGESET_MEMO_BYTES: usize = 32 << 20;
/// Bytes of recently read or written derived pointers kept in memory.
const DERIVED_MEMO_BYTES: usize = 16 << 20;
/// Bytes of repository ids kept in memory.
const REPO_MEMO_BYTES: usize = 1 << 20;
/// Bytes of recently read or written trees kept in memory: deriving a
/// changeset reads back the directories its parent just wrote.
const TREE_MEMO_BYTES: usize = 64 << 20;
/// Bytes of recently read or written path-history pages kept in memory.
const PAGE_MEMO_BYTES: usize = 64 << 20;

/// Bytes of repositories' refs kept in memory while listening.
const REF_MEMO_BYTES: usize = 1 << 20;
/// Bytes of repositories' grants kept in memory while listening.
const GRANT_MEMO_BYTES: usize = 4 << 20;
/// The channel on which every write to `refs` or `grants` names its
/// repository, in the writing transaction.
const INVALIDATE_CHANNEL: &str = "scion_invalidate";
/// Pause before reconnecting a listener whose connection was lost.
const RELISTEN_DELAY: Duration = Duration::from_secs(1);

type DerivedKey = (String, String, u32, ChangesetId);

/// A [`MetadataStore`] backed by Postgres.
///
/// Changesets, derived pointers, trees, pages and repository ids are
/// write-once, so the recently read or written ones are memoised in a bounded
/// amount of memory. Refs and grants are memoised per repository only while
/// [`listen`](Self::listen)ing. Clones share the memo.
#[derive(Debug, Clone)]
pub struct PostgresStore {
    pool: PgPool,
    repo_ids: Arc<Recent<String, i64>>,
    changesets: Arc<Recent<(String, ChangesetId), Changeset>>,
    derived: Arc<Recent<DerivedKey, Vec<u8>>>,
    trees: Arc<Recent<(String, TreeId), Tree>>,
    pages: Arc<Recent<(String, BlobId), Bytes>>,
    mutable: Arc<Mutable>,
}

/// Memos of each repository's refs and grants, which other processes write
/// too. They serve reads only while a listener receives the notifications
/// those writes send; each notification drops its repository's entries.
#[derive(Debug)]
struct Mutable {
    refs: Recent<String, Arc<Vec<(String, RefState)>>>,
    grants: Recent<String, Arc<Vec<Grant>>>,
    /// Bumped by every invalidation, and whether a listener is connected.
    /// A read remembers its result only if no invalidation happened since
    /// it started, so a result older than a write is never kept after it.
    epoch: Mutex<(u64, bool)>,
}

impl Mutable {
    fn new() -> Self {
        Self {
            refs: Recent::new(REF_MEMO_BYTES),
            grants: Recent::new(GRANT_MEMO_BYTES),
            epoch: Mutex::new((0, false)),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, (u64, bool)> {
        self.epoch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The current epoch, if the memos are in use.
    fn epoch(&self) -> Option<u64> {
        let (epoch, listening) = *self.state();
        listening.then_some(epoch)
    }

    /// Run `remember` unless an invalidation happened since `epoch`.
    fn remember(&self, epoch: u64, remember: impl FnOnce()) {
        let state = self.state();
        if *state == (epoch, true) {
            remember();
        }
    }

    /// Drop `repo`'s entries.
    fn forget(&self, repo: &str) {
        let mut state = self.state();
        state.0 += 1;
        self.refs.remove(&repo.to_owned());
        self.grants.remove(&repo.to_owned());
    }

    /// Drop every entry, and use the memos from now on only if `listening`.
    fn reset(&self, listening: bool) {
        let mut state = self.state();
        *state = (state.0 + 1, listening);
        self.refs.clear();
        self.grants.clear();
    }
}

/// `repo`'s memoised value in `memo`, or `read` and remember it, weighed by
/// `weight`, while the memos are in use.
async fn memoized<V: Clone + Send + Sync + 'static>(
    mutable: &Mutable,
    memo: &Recent<String, V>,
    repo: &str,
    weight: impl FnOnce(&V) -> usize,
    read: impl Future<Output = Result<V>>,
) -> Result<V> {
    let Some(epoch) = mutable.epoch() else {
        return read.await;
    };
    if let Some(hit) = memo.get(&repo.to_owned()).await {
        return Ok(hit);
    }
    let value = read.await?;
    mutable.remember(epoch, || {
        memo.insert(repo.to_owned(), value.clone(), repo.len() + weight(&value));
    });
    Ok(value)
}

/// Rows of `changesets.canonical` one blocking task decodes: a larger read
/// decodes on several cores at once, so a 1,000-result log does not decode
/// on one.
const DECODE_CHUNK: usize = 64;

/// Decode `rows` of `changesets.canonical`, with each row's id and length.
async fn decode_changesets(rows: Vec<Vec<u8>>) -> Result<Vec<(ChangesetId, Changeset, usize)>> {
    fn decode(rows: Vec<Vec<u8>>) -> Result<Vec<(ChangesetId, Changeset, usize)>> {
        rows.into_iter()
            .map(|canonical| {
                let cs = Changeset::decode(&canonical)
                    .map_err(|e| StoreError::Corrupt(format!("changeset: {e}")))?;
                // A value has exactly one canonical encoding, so the stored
                // bytes hash to the id without encoding them again.
                Ok((ChangesetId::hash(&canonical), cs, canonical.len()))
            })
            .collect()
    }
    if rows.len() <= DECODE_CHUNK {
        return decode(rows);
    }
    let mut rows = rows.into_iter().peekable();
    let mut chunks = Vec::new();
    while rows.peek().is_some() {
        let chunk: Vec<_> = rows.by_ref().take(DECODE_CHUNK).collect();
        chunks.push(tokio::task::spawn_blocking(move || decode(chunk)));
    }
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(
            chunk
                .await
                .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))?,
        );
    }
    Ok(out)
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        StoreError::Backend(Box::new(e))
    }
}

impl From<sqlx::migrate::MigrateError> for StoreError {
    fn from(e: sqlx::migrate::MigrateError) -> Self {
        StoreError::Backend(Box::new(e))
    }
}

fn to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| StoreError::Corrupt(format!("value {v} does not fit in BIGINT")))
}

fn to_u64(v: i64) -> Result<u64> {
    u64::try_from(v).map_err(|_| StoreError::Corrupt(format!("negative BIGINT {v}")))
}

impl PostgresStore {
    /// Connect using a URL such as `postgres://user:pass@host/db`.
    pub async fn connect(url: &str) -> Result<Self> {
        let opts: PgConnectOptions = url.parse()?;
        Self::connect_with(opts).await
    }

    /// Connect with explicit options.
    pub async fn connect_with(opts: PgConnectOptions) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect_with(opts)
            .await?;
        Ok(Self::from_pool(pool))
    }

    /// Wrap an existing pool.
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            repo_ids: Arc::new(Recent::new(REPO_MEMO_BYTES)),
            changesets: Arc::new(Recent::new(CHANGESET_MEMO_BYTES)),
            derived: Arc::new(Recent::new(DERIVED_MEMO_BYTES)),
            trees: Arc::new(Recent::new(TREE_MEMO_BYTES)),
            pages: Arc::new(Recent::new(PAGE_MEMO_BYTES)),
            mutable: Arc::new(Mutable::new()),
        }
    }

    /// Memoise refs and grants from now on, on a connection of its own that
    /// listens for the notifications every write to them sends, from any
    /// process. A notification drops its repository's entries. While that
    /// connection is lost, reads go to the database and the memo is empty;
    /// it is reconnected in the background.
    pub async fn listen(&self) -> Result<()> {
        let listener = self.listener().await?;
        let store = self.clone();
        tokio::spawn(async move {
            let mut listener = Some(listener);
            loop {
                if let Some(mut l) = listener.take() {
                    // `Ok(None)` and `Err` both mean the connection is gone,
                    // with whatever was sent meanwhile.
                    while let Ok(Some(n)) = l.try_recv().await {
                        store.mutable.forget(n.payload());
                    }
                    store.mutable.reset(false);
                }
                tokio::time::sleep(RELISTEN_DELAY).await;
                listener = store.listener().await.ok();
            }
        });
        Ok(())
    }

    /// A connection listening on [`INVALIDATE_CHANNEL`], outside the pool,
    /// after which the memos start out empty.
    async fn listener(&self) -> Result<PgListener> {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with((*self.pool.connect_options()).clone());
        let mut listener = PgListener::connect_with(&pool).await?;
        listener.eager_reconnect(false);
        listener.listen(INVALIDATE_CHANNEL).await?;
        self.mutable.reset(true);
        Ok(listener)
    }

    /// Apply pending migrations.
    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// The underlying pool.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Look up a repository id without creating it.
    async fn repo_id<'e, E>(exec: E, name: &str) -> Result<Option<i64>>
    where
        E: Executor<'e, Database = Postgres>,
    {
        let id: Option<(i64,)> = sqlx::query_as("SELECT id FROM repos WHERE name = $1")
            .bind(name)
            .fetch_optional(exec)
            .await?;
        Ok(id.map(|(id,)| id))
    }

    /// The id of repository `name`; [`StoreError::RepoNotFound`] if it was
    /// never created. A repository row is never deleted, so its id is
    /// memoised.
    async fn repo(&self, name: &str) -> Result<i64> {
        if name == SERVER_GRANTS {
            return Err(StoreError::RepoNotFound(name.to_owned()));
        }
        self.grant_repo(name).await
    }

    /// [`repo`](Self::repo), also accepting [`SERVER_GRANTS`].
    async fn grant_repo(&self, name: &str) -> Result<i64> {
        if let Some(id) = self.repo_ids.get(&name.to_owned()).await {
            return Ok(id);
        }
        let id = Self::repo_id(&self.pool, name)
            .await?
            .ok_or_else(|| StoreError::RepoNotFound(name.to_owned()))?;
        self.repo_ids.insert(name.to_owned(), id, name.len() + 8);
        Ok(id)
    }
}

/// Most changeset rows in one `INSERT`, well under Postgres' 65,535 bind
/// parameters at 15 per row.
const CHANGESET_ROWS_PER_INSERT: usize = 1000;

/// A changeset row to insert: repository id, changeset id, generation, the
/// changeset and its canonical encoding.
type ChangesetRow<'a> = (i64, ChangesetId, i64, &'a Changeset, Vec<u8>);

/// Insert `rows` into `changesets`, and every one of their file changes into
/// `file_changes`.
async fn insert_changesets(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    rows: &[ChangesetRow<'_>],
) -> Result<()> {
    for chunk in rows.chunks(CHANGESET_ROWS_PER_INSERT) {
        let mut insert = sqlx::QueryBuilder::<Postgres>::new(
            "INSERT INTO changesets (repo, id, parents, first_parent, author_name, author_email, \
             author_date, author_tz, committer_name, committer_email, committer_date, \
             committer_tz, message, generation, canonical) ",
        );
        insert.push_values(chunk, |mut row, (repo, id, generation, cs, canonical)| {
            let parents: Vec<Vec<u8>> = cs.parents.iter().map(|p| p.as_bytes().to_vec()).collect();
            row.push_bind(*repo)
                .push_bind(id.as_bytes().to_vec())
                .push_bind(parents)
                .push_bind(cs.first_parent().map(|p| p.as_bytes().to_vec()))
                .push_bind(cs.author.name.clone())
                .push_bind(cs.author.email.clone())
                .push_bind(cs.author_date.unix_secs)
                .push_bind(cs.author_date.tz_offset_minutes)
                .push_bind(cs.committer.as_ref().map(|c| c.name.clone()))
                .push_bind(cs.committer.as_ref().map(|c| c.email.clone()))
                .push_bind(cs.committer_date.map(|t| t.unix_secs))
                .push_bind(cs.committer_date.map(|t| t.tz_offset_minutes))
                .push_bind(cs.message.clone())
                .push_bind(*generation)
                .push_bind(canonical.clone());
        });
        // A concurrent writer of the same changeset may commit between our
        // read and this insert; content addressing makes its row ours.
        insert.push(" ON CONFLICT (repo, id) DO NOTHING");
        insert.build().execute(&mut **tx).await?;
    }

    // One statement for every file change of the batch: an imported root
    // commit touches thousands of paths, and a round trip each would dominate.
    let n = rows
        .iter()
        .map(|(_, _, _, cs, _)| cs.file_changes.len())
        .sum();
    if n == 0 {
        return Ok(());
    }
    let mut cols = FileChangeColumns::with_capacity(n);
    for (repo, id, _, cs, _) in rows {
        for (path, change) in &cs.file_changes {
            cols.push(*repo, *id, path.as_str(), change)?;
        }
    }
    sqlx::query(
        "INSERT INTO file_changes (repo, changeset_id, path, change_kind, blob_id, file_type, \
         size, copy_from_path, copy_from_cs) \
         SELECT * FROM UNNEST($1::bigint[], $2::bytea[], $3::text[], $4::smallint[], \
         $5::bytea[], $6::smallint[], $7::bigint[], $8::text[], $9::bytea[]) \
         ON CONFLICT (repo, changeset_id, path) DO NOTHING",
    )
    .bind(&cols.repos)
    .bind(&cols.changesets)
    .bind(&cols.paths)
    .bind(&cols.kinds)
    .bind(&cols.blobs)
    .bind(&cols.ftypes)
    .bind(&cols.sizes)
    .bind(&cols.cf_paths)
    .bind(&cols.cf_css)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// `file_changes` rows as one array per column, for `UNNEST`.
struct FileChangeColumns<'a> {
    repos: Vec<i64>,
    changesets: Vec<Vec<u8>>,
    paths: Vec<&'a str>,
    kinds: Vec<i16>,
    blobs: Vec<Option<Vec<u8>>>,
    ftypes: Vec<Option<i16>>,
    sizes: Vec<Option<i64>>,
    cf_paths: Vec<Option<&'a str>>,
    cf_css: Vec<Option<Vec<u8>>>,
}

impl<'a> FileChangeColumns<'a> {
    fn with_capacity(n: usize) -> Self {
        Self {
            repos: Vec::with_capacity(n),
            changesets: Vec::with_capacity(n),
            paths: Vec::with_capacity(n),
            kinds: Vec::with_capacity(n),
            blobs: Vec::with_capacity(n),
            ftypes: Vec::with_capacity(n),
            sizes: Vec::with_capacity(n),
            cf_paths: Vec::with_capacity(n),
            cf_css: Vec::with_capacity(n),
        }
    }

    fn push(
        &mut self,
        repo: i64,
        cs: ChangesetId,
        path: &'a str,
        change: &'a FileChange,
    ) -> Result<()> {
        self.repos.push(repo);
        self.changesets.push(cs.as_bytes().to_vec());
        self.paths.push(path);
        match change {
            FileChange::Deleted => {
                self.kinds.push(0);
                self.blobs.push(None);
                self.ftypes.push(None);
                self.sizes.push(None);
                self.cf_paths.push(None);
                self.cf_css.push(None);
            }
            FileChange::Tracked {
                content,
                kind,
                size,
                copy_from,
            } => {
                self.kinds.push(1);
                self.blobs.push(Some(content.as_bytes().to_vec()));
                self.ftypes.push(Some(i16::from(kind.tag())));
                self.sizes.push(Some(to_i64(*size)?));
                self.cf_paths
                    .push(copy_from.as_ref().map(|(p, _)| p.as_str()));
                self.cf_css
                    .push(copy_from.as_ref().map(|(_, c)| c.as_bytes().to_vec()));
            }
        }
        Ok(())
    }
}

/// The `change_requests` columns every query returns, in
/// [`ChangeRequestRow`] order.
/// `submitted_at` and `landed_at` come back in microseconds since the Unix
/// epoch, their precision.
const CHANGE_REQUEST_COLUMNS: &str = "c.id, c.head_cs, c.base_cs, c.submitter, c.status::text, c.landed_cs, \
     c.conflicts, (EXTRACT(EPOCH FROM c.submitted_at) * 1000000)::bigint, \
     (EXTRACT(EPOCH FROM c.landed_at) * 1000000)::bigint";

type ChangeRequestRow = (
    i64,
    Vec<u8>,
    Vec<u8>,
    String,
    String,
    Option<Vec<u8>>,
    Option<Vec<String>>,
    i64,
    Option<i64>,
);

fn change_request(
    (id, head, base, submitter, status, landed, conflicts, submitted_us, landed_us): ChangeRequestRow,
) -> Result<ChangeRequest> {
    let time = |us: i64| to_u64(us).map(|us| UNIX_EPOCH + Duration::from_micros(us));
    let cs = |bytes: &[u8]| {
        ChangesetId::try_from_slice(bytes)
            .map_err(|e| StoreError::Corrupt(format!("change request {id}: {e}")))
    };
    Ok(ChangeRequest {
        id: to_u64(id)?,
        head: cs(&head)?,
        base: cs(&base)?,
        submitter,
        status: ChangeRequestStatus::parse(&status)
            .ok_or_else(|| StoreError::Corrupt(format!("change request status {status:?}")))?,
        landed: landed.as_deref().map(cs).transpose()?,
        conflicts: conflicts.unwrap_or_default(),
        submitted_at: time(submitted_us)?,
        landed_at: landed_us.map(time).transpose()?,
    })
}

/// The statement behind [`MetadataStore::find_landings`]: per asked
/// changeset, one probe of `change_requests_landed_cs` and one of
/// `change_requests_landed_head`, keeping the oldest match.
fn find_landings_sql() -> String {
    format!(
        "SELECT {CHANGE_REQUEST_COLUMNS} FROM unnest($2::bytea[]) AS q(cs) \
         CROSS JOIN LATERAL (SELECT * FROM change_requests r WHERE r.repo = $1 \
         AND r.status = 'landed' AND (r.landed_cs = q.cs OR r.head_cs = q.cs) \
         ORDER BY r.id LIMIT 1) c"
    )
}

/// The tree `id` from its stored canonical encoding, checked against its id.
fn decode_tree(id: TreeId, bytes: &[u8]) -> Result<Tree> {
    let tree = Tree::decode(bytes).map_err(|e| StoreError::Corrupt(format!("tree {id}: {e}")))?;
    if tree.id() != id {
        return Err(StoreError::Corrupt(format!(
            "tree {id} hashes to {}",
            tree.id()
        )));
    }
    Ok(tree)
}

impl PostgresStore {
    /// Every ref of `repo`, by name, memoised while listening.
    async fn refs(&self, repo: &str) -> Result<Arc<Vec<(String, RefState)>>> {
        let weight = |refs: &Arc<Vec<(String, RefState)>>| {
            refs.iter().map(|(name, _)| name.len() + 48).sum()
        };
        memoized(&self.mutable, &self.mutable.refs, repo, weight, async {
            Ok(Arc::new(self.select_refs(repo).await?))
        })
        .await
    }

    async fn select_refs(&self, repo: &str) -> Result<Vec<(String, RefState)>> {
        let rows: Vec<(String, Vec<u8>, i64)> = sqlx::query_as(
            "SELECT f.name, f.target, f.version FROM refs f JOIN repos r ON r.id = f.repo \
             WHERE r.name = $1 ORDER BY f.name",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(n, t, v)| {
                Ok((
                    n,
                    RefState {
                        target: ChangesetId::try_from_slice(&t)
                            .map_err(|e| StoreError::Corrupt(e.to_string()))?,
                        version: to_u64(v)?,
                    },
                ))
            })
            .collect()
    }

    /// Every grant of `repo`, by principal and path prefix, memoised while
    /// listening.
    async fn repo_grants(&self, repo: &str) -> Result<Arc<Vec<Grant>>> {
        let weight = |grants: &Arc<Vec<Grant>>| {
            grants
                .iter()
                .map(|g| g.principal.len() + g.path_prefix.len() + 8)
                .sum()
        };
        memoized(&self.mutable, &self.mutable.grants, repo, weight, async {
            Ok(Arc::new(self.select_grants(repo).await?))
        })
        .await
    }

    async fn select_grants(&self, repo: &str) -> Result<Vec<Grant>> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT g.principal, g.path_prefix, g.role FROM grants g JOIN repos r ON r.id = g.repo \
             WHERE r.name = $1 ORDER BY g.principal COLLATE \"C\", g.path_prefix COLLATE \"C\"",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(principal, path_prefix, role)| {
                let role = Role::parse(&role)
                    .ok_or_else(|| StoreError::Corrupt(format!("grant role {role:?}")))?;
                Ok(Grant {
                    principal,
                    path_prefix,
                    role,
                })
            })
            .collect()
    }

    /// Insert `(id, bytes)` rows of `repo` into `table` (`trees` or `pages`)
    /// in one statement; rows already stored are kept.
    async fn insert_objects(
        &self,
        table: &str,
        repo: &str,
        ids: Vec<Vec<u8>>,
        bytes: Vec<&[u8]>,
    ) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let repo_id = self.repo(repo).await?;
        sqlx::query(&format!(
            "INSERT INTO {table} (repo, id, bytes) \
             SELECT $1, * FROM UNNEST($2::bytea[], $3::bytea[]) \
             ON CONFLICT (repo, id) DO NOTHING"
        ))
        .bind(repo_id)
        .bind(&ids)
        .bind(&bytes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The `(id, bytes)` rows of `repo` in `table` among `ids`, in one query.
    async fn select_objects(
        &self,
        table: &str,
        repo: &str,
        ids: &[Vec<u8>],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        Ok(sqlx::query_as(&format!(
            "SELECT t.id, t.bytes FROM {table} t JOIN repos r ON r.id = t.repo \
             WHERE r.name = $1 AND t.id = ANY($2)"
        ))
        .bind(repo)
        .bind(ids)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[async_trait]
impl MetadataStore for PostgresStore {
    async fn put_trees(&self, repo: &str, trees: &[Tree]) -> Result<()> {
        let mut fresh = Vec::new();
        for tree in trees {
            let id = tree.id();
            if self.trees.get(&(repo.to_owned(), id)).await.is_none() {
                fresh.push((id, tree, tree.encode_to_vec()));
            }
        }
        self.insert_objects(
            "trees",
            repo,
            fresh
                .iter()
                .map(|(id, _, _)| id.as_bytes().to_vec())
                .collect(),
            fresh.iter().map(|(_, _, bytes)| bytes.as_slice()).collect(),
        )
        .await?;
        for (id, tree, bytes) in fresh {
            self.trees
                .insert((repo.to_owned(), id), tree.clone(), bytes.len());
        }
        Ok(())
    }

    async fn get_trees(&self, repo: &str, ids: &[TreeId]) -> Result<Vec<Option<Tree>>> {
        let mut out = Vec::with_capacity(ids.len());
        let mut missing = Vec::new();
        for id in ids {
            let tree = self.trees.get(&(repo.to_owned(), *id)).await;
            if tree.is_none() {
                missing.push(id.as_bytes().to_vec());
            }
            out.push(tree);
        }
        if missing.is_empty() {
            return Ok(out);
        }
        let mut found = HashMap::new();
        for (id, bytes) in self.select_objects("trees", repo, &missing).await? {
            let id = TreeId::try_from_slice(&id)
                .map_err(|e| StoreError::Corrupt(format!("tree id: {e}")))?;
            let tree = decode_tree(id, &bytes)?;
            self.trees
                .insert((repo.to_owned(), id), tree.clone(), bytes.len());
            found.insert(id, tree);
        }
        for (slot, id) in out.iter_mut().zip(ids) {
            if slot.is_none() {
                *slot = found.get(id).cloned();
            }
        }
        Ok(out)
    }

    async fn put_pages(&self, repo: &str, pages: &[Bytes]) -> Result<()> {
        let mut fresh = Vec::new();
        for page in pages {
            let id = BlobId::hash(page);
            if self.pages.get(&(repo.to_owned(), id)).await.is_none() {
                fresh.push((id, page));
            }
        }
        self.insert_objects(
            "pages",
            repo,
            fresh.iter().map(|(id, _)| id.as_bytes().to_vec()).collect(),
            fresh.iter().map(|(_, page)| page.as_ref()).collect(),
        )
        .await?;
        for (id, page) in fresh {
            self.pages
                .insert((repo.to_owned(), id), page.clone(), page.len());
        }
        Ok(())
    }

    async fn get_pages(&self, repo: &str, ids: &[BlobId]) -> Result<Vec<Option<Bytes>>> {
        let mut out = Vec::with_capacity(ids.len());
        let mut missing = Vec::new();
        for id in ids {
            let page = self.pages.get(&(repo.to_owned(), *id)).await;
            if page.is_none() {
                missing.push(id.as_bytes().to_vec());
            }
            out.push(page);
        }
        if missing.is_empty() {
            return Ok(out);
        }
        let mut found = HashMap::new();
        for (id, bytes) in self.select_objects("pages", repo, &missing).await? {
            let id = BlobId::try_from_slice(&id)
                .map_err(|e| StoreError::Corrupt(format!("page id: {e}")))?;
            if BlobId::hash(&bytes) != id {
                return Err(StoreError::Corrupt(format!(
                    "page {id} does not match its content"
                )));
            }
            let page = Bytes::from(bytes);
            self.pages
                .insert((repo.to_owned(), id), page.clone(), page.len());
            found.insert(id, page);
        }
        for (slot, id) in out.iter_mut().zip(ids) {
            if slot.is_none() {
                *slot = found.get(id).cloned();
            }
        }
        Ok(out)
    }

    async fn create_repo(&self, name: &str) -> Result<()> {
        if name == SERVER_GRANTS {
            return Err(StoreError::ReservedRepoName(name.to_owned()));
        }
        sqlx::query("INSERT INTO repos (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn put_changesets(&self, batch: &[Changeset]) -> Result<()> {
        let mut repos: HashMap<&str, i64> = HashMap::new();
        for cs in batch {
            if !repos.contains_key(cs.repo.as_str()) {
                repos.insert(&cs.repo, self.repo(&cs.repo).await?);
            }
        }
        let ids: Vec<(i64, ChangesetId)> = batch
            .iter()
            .map(|cs| (repos[cs.repo.as_str()], cs.id()))
            .collect();

        let mut tx = self.pool.begin().await?;
        // One read finds which changesets are already stored and the
        // generation of every parent outside the batch.
        let (mut key_repos, mut key_ids) = (Vec::new(), Vec::new());
        for (cs, (repo, id)) in batch.iter().zip(&ids) {
            for key in std::iter::once(id).chain(&cs.parents) {
                key_repos.push(*repo);
                key_ids.push(key.as_bytes().to_vec());
            }
        }
        let known: Vec<(i64, Vec<u8>, i64)> = sqlx::query_as(
            "SELECT c.repo, c.id, c.generation FROM changesets c \
             JOIN UNNEST($1::bigint[], $2::bytea[]) AS k(repo, id) \
             ON c.repo = k.repo AND c.id = k.id",
        )
        .bind(&key_repos)
        .bind(&key_ids)
        .fetch_all(&mut *tx)
        .await?;
        let mut generations: HashMap<(i64, ChangesetId), i64> = HashMap::new();
        for (repo, id, generation) in known {
            let id = ChangesetId::try_from_slice(&id)
                .map_err(|e| StoreError::Corrupt(format!("changeset id: {e}")))?;
            generations.insert((repo, id), generation);
        }

        // Generation = max(parent generation) + 1; every parent must exist.
        let mut rows = Vec::new();
        for (cs, &(repo, id)) in batch.iter().zip(&ids) {
            if generations.contains_key(&(repo, id)) {
                continue;
            }
            let mut generation = 0i64;
            for p in &cs.parents {
                match generations.get(&(repo, *p)) {
                    Some(g) => generation = generation.max(g + 1),
                    None => return Err(StoreError::UnknownParent(id, *p)),
                }
            }
            generations.insert((repo, id), generation);
            rows.push((repo, id, generation, cs, cs.encode_to_vec()));
        }
        if rows.is_empty() {
            tx.rollback().await?;
            return Ok(());
        }
        insert_changesets(&mut tx, &rows).await?;
        tx.commit().await?;
        for (_, id, _, cs, canonical) in rows {
            self.changesets
                .insert((cs.repo.clone(), id), cs.clone(), canonical.len());
        }
        Ok(())
    }

    async fn get_changeset(&self, repo: &str, id: ChangesetId) -> Result<Option<Changeset>> {
        let key = (repo.to_owned(), id);
        if let Some(cs) = self.changesets.get(&key).await {
            return Ok(Some(cs));
        }
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT c.canonical FROM changesets c JOIN repos r ON r.id = c.repo \
             WHERE r.name = $1 AND c.id = $2",
        )
        .bind(repo)
        .bind(id.as_bytes().as_slice())
        .fetch_optional(&self.pool)
        .await?;
        let Some((canonical,)) = row else {
            return Ok(None);
        };
        let cs = Changeset::decode(&canonical)
            .map_err(|e| StoreError::Corrupt(format!("changeset {id}: {e}")))?;
        if cs.id() != id {
            return Err(StoreError::Corrupt(format!(
                "changeset {id} hashes to {}",
                cs.id()
            )));
        }
        self.changesets.insert(key, cs.clone(), canonical.len());
        Ok(Some(cs))
    }

    async fn get_changesets(
        &self,
        repo: &str,
        ids: &[ChangesetId],
    ) -> Result<Vec<Option<Changeset>>> {
        let mut out = Vec::with_capacity(ids.len());
        let mut missing = Vec::new();
        for id in ids {
            let cs = self.changesets.get(&(repo.to_owned(), *id)).await;
            if cs.is_none() {
                missing.push(id.as_bytes().to_vec());
            }
            out.push(cs);
        }
        if missing.is_empty() {
            return Ok(out);
        }
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
            "SELECT c.canonical FROM changesets c JOIN repos r ON r.id = c.repo \
             WHERE r.name = $1 AND c.id = ANY($2)",
        )
        .bind(repo)
        .bind(&missing)
        .fetch_all(&self.pool)
        .await?;
        let mut found = HashMap::new();
        for (id, cs, len) in decode_changesets(rows.into_iter().map(|(c,)| c).collect()).await? {
            self.changesets
                .insert((repo.to_owned(), id), cs.clone(), len);
            found.insert(id, cs);
        }
        for (slot, id) in out.iter_mut().zip(ids) {
            if slot.is_none() {
                *slot = found.get(id).cloned();
            }
        }
        Ok(out)
    }

    async fn generation(&self, repo: &str, id: ChangesetId) -> Result<Option<u64>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT c.generation FROM changesets c JOIN repos r ON r.id = c.repo \
             WHERE r.name = $1 AND c.id = $2",
        )
        .bind(repo)
        .bind(id.as_bytes().as_slice())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(g,)| to_u64(g)).transpose()
    }

    async fn create_ref(&self, repo: &str, name: &str, target: ChangesetId) -> Result<RefState> {
        if !is_allowed_ref_name(name) {
            return Err(StoreError::RefNameNotAllowed(name.to_owned()));
        }
        let repo_id = self.repo(repo).await?;
        let inserted: Option<(String,)> = sqlx::query_as(
            "WITH i AS (INSERT INTO refs (repo, name, target, version) VALUES ($1, $2, $3, 0) \
             ON CONFLICT (repo, name) DO NOTHING RETURNING 1) \
             SELECT pg_notify($4, $5)::text FROM i",
        )
        .bind(repo_id)
        .bind(name)
        .bind(target.as_bytes().as_slice())
        .bind(INVALIDATE_CHANNEL)
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;
        if inserted.is_none() {
            return Err(StoreError::RefExists(name.to_owned()));
        }
        self.mutable.forget(repo);
        Ok(RefState { target, version: 0 })
    }

    async fn get_ref(&self, repo: &str, name: &str) -> Result<Option<RefState>> {
        Ok(self
            .refs(repo)
            .await?
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, state)| *state))
    }

    async fn move_ref(
        &self,
        repo: &str,
        name: &str,
        expected_version: u64,
        new_target: ChangesetId,
    ) -> Result<RefState> {
        let expected = to_i64(expected_version)?;
        // Single-statement compare-and-swap.
        let updated: Option<(i64, String)> = sqlx::query_as(
            "WITH u AS (UPDATE refs f SET target = $3, version = f.version + 1, \
             updated_at = now() FROM repos r \
             WHERE r.id = f.repo AND r.name = $1 AND f.name = $2 AND f.version = $4 \
             RETURNING f.version) \
             SELECT version, pg_notify($5, $1)::text FROM u",
        )
        .bind(repo)
        .bind(name)
        .bind(new_target.as_bytes().as_slice())
        .bind(expected)
        .bind(INVALIDATE_CHANNEL)
        .fetch_optional(&self.pool)
        .await?;
        // On a mismatch the memo may predate the write that won.
        self.mutable.forget(repo);
        if let Some((version, _)) = updated {
            return Ok(RefState {
                target: new_target,
                version: to_u64(version)?,
            });
        }
        match self.get_ref(repo, name).await? {
            None => {
                self.repo(repo).await?;
                Err(StoreError::RefNotFound(name.to_owned()))
            }
            Some(state) => Err(StoreError::CasMismatch {
                expected: expected_version,
                actual: state.version,
            }),
        }
    }

    async fn list_refs(&self, repo: &str) -> Result<Vec<(String, RefState)>> {
        Ok(self.refs(repo).await?.to_vec())
    }

    async fn put_derived_many(&self, repo: &str, pointers: Vec<DerivedPointer>) -> Result<()> {
        if pointers.is_empty() {
            return Ok(());
        }
        let repo_id = self.repo(repo).await?;
        let mut types = Vec::with_capacity(pointers.len());
        let mut versions = Vec::with_capacity(pointers.len());
        let mut css = Vec::with_capacity(pointers.len());
        let mut values = Vec::with_capacity(pointers.len());
        for (ty, version, cs, pointer) in &pointers {
            types.push(ty.as_str());
            versions.push(
                i32::try_from(*version)
                    .map_err(|_| StoreError::Corrupt("derived version overflow".into()))?,
            );
            css.push(cs.as_bytes().to_vec());
            values.push(pointer.as_slice());
        }
        sqlx::query(
            "INSERT INTO derived (repo, type, version, changeset_id, pointer) \
             SELECT $1, * FROM UNNEST($2::text[], $3::int[], $4::bytea[], $5::bytea[]) \
             ON CONFLICT DO NOTHING",
        )
        .bind(repo_id)
        .bind(&types)
        .bind(&versions)
        .bind(&css)
        .bind(&values)
        .execute(&self.pool)
        .await?;
        // A conflicting insert kept the first pointer; both are the same
        // value of a deterministic derivation, so either may be memoised.
        for (ty, version, cs, pointer) in pointers {
            let weight = pointer.len() + ty.len() + repo.len() + 40;
            self.derived
                .insert((repo.to_owned(), ty, version, cs), pointer, weight);
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
        let key = (repo.to_owned(), ty.to_owned(), version, cs);
        if let Some(pointer) = self.derived.get(&key).await {
            return Ok(Some(pointer));
        }
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT d.pointer FROM derived d JOIN repos r ON r.id = d.repo \
             WHERE r.name = $1 AND d.type = $2 AND d.version = $3 AND d.changeset_id = $4",
        )
        .bind(repo)
        .bind(ty)
        .bind(
            i32::try_from(version)
                .map_err(|_| StoreError::Corrupt("derived version overflow".into()))?,
        )
        .bind(cs.as_bytes().as_slice())
        .fetch_optional(&self.pool)
        .await?;
        if let Some((pointer,)) = &row {
            let weight = pointer.len() + ty.len() + repo.len() + 40;
            self.derived.insert(key, pointer.clone(), weight);
        }
        Ok(row.map(|(p,)| p))
    }

    async fn get_derived_many(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        ids: &[ChangesetId],
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let key = |cs: ChangesetId| (repo.to_owned(), ty.to_owned(), version, cs);
        let mut out = Vec::with_capacity(ids.len());
        let mut missing = Vec::new();
        for id in ids {
            let pointer = self.derived.get(&key(*id)).await;
            if pointer.is_none() {
                missing.push(id.as_bytes().to_vec());
            }
            out.push(pointer);
        }
        if missing.is_empty() {
            return Ok(out);
        }
        let rows: Vec<(Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT d.changeset_id, d.pointer FROM derived d JOIN repos r ON r.id = d.repo \
             WHERE r.name = $1 AND d.type = $2 AND d.version = $3 AND d.changeset_id = ANY($4)",
        )
        .bind(repo)
        .bind(ty)
        .bind(
            i32::try_from(version)
                .map_err(|_| StoreError::Corrupt("derived version overflow".into()))?,
        )
        .bind(&missing)
        .fetch_all(&self.pool)
        .await?;
        let mut found = HashMap::new();
        for (cs, pointer) in rows {
            let cs = ChangesetId::try_from_slice(&cs)
                .map_err(|e| StoreError::Corrupt(format!("changeset id: {e}")))?;
            let weight = pointer.len() + ty.len() + repo.len() + 40;
            self.derived.insert(key(cs), pointer.clone(), weight);
            found.insert(cs, pointer);
        }
        for (slot, id) in out.iter_mut().zip(ids) {
            if slot.is_none() {
                *slot = found.get(id).cloned();
            }
        }
        Ok(out)
    }

    async fn put_ingest_checkpoints(
        &self,
        repo: &str,
        done: &[(Vec<u8>, ChangesetId)],
    ) -> Result<()> {
        if done.is_empty() {
            return Ok(());
        }
        let repo_id = self.repo(repo).await?;
        let shas: Vec<&[u8]> = done.iter().map(|(sha, _)| sha.as_slice()).collect();
        let css: Vec<&[u8]> = done
            .iter()
            .map(|(_, cs)| cs.as_bytes().as_slice())
            .collect();
        sqlx::query(
            "INSERT INTO ingest_checkpoints (repo, git_sha, changeset_id) \
             SELECT $1, * FROM UNNEST($2::bytea[], $3::bytea[]) ON CONFLICT DO NOTHING",
        )
        .bind(repo_id)
        .bind(&shas)
        .bind(&css)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn ingest_checkpoints(&self, repo: &str) -> Result<Vec<(Vec<u8>, ChangesetId)>> {
        let rows: Vec<(Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT c.git_sha, c.changeset_id FROM ingest_checkpoints c \
             JOIN repos r ON r.id = c.repo WHERE r.name = $1 ORDER BY c.git_sha",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(sha, cs)| {
                let cs = ChangesetId::try_from_slice(&cs)
                    .map_err(|e| StoreError::Corrupt(format!("checkpoint changeset id: {e}")))?;
                Ok((sha, cs))
            })
            .collect()
    }

    async fn submit_change_request(
        &self,
        repo: &str,
        head: ChangesetId,
        base: ChangesetId,
        submitter: &str,
    ) -> Result<ChangeRequest> {
        let repo_id = self.repo(repo).await?;
        let row: ChangeRequestRow = sqlx::query_as(&format!(
            "INSERT INTO change_requests AS c (repo, head_cs, base_cs, submitter) \
             VALUES ($1, $2, $3, $4) \
             RETURNING {CHANGE_REQUEST_COLUMNS}"
        ))
        .bind(repo_id)
        .bind(head.as_bytes().as_slice())
        .bind(base.as_bytes().as_slice())
        .bind(submitter)
        .fetch_one(&self.pool)
        .await?;
        change_request(row)
    }

    async fn get_change_request(&self, repo: &str, id: u64) -> Result<Option<ChangeRequest>> {
        let row: Option<ChangeRequestRow> = sqlx::query_as(&format!(
            "SELECT {CHANGE_REQUEST_COLUMNS} FROM change_requests c JOIN repos r ON r.id = c.repo \
             WHERE r.name = $1 AND c.id = $2"
        ))
        .bind(repo)
        .bind(to_i64(id)?)
        .fetch_optional(&self.pool)
        .await?;
        row.map(change_request).transpose()
    }

    async fn list_change_requests(
        &self,
        repo: &str,
        status: Option<ChangeRequestStatus>,
        limit: u32,
    ) -> Result<Vec<ChangeRequest>> {
        // Backwards through `change_requests_status` with a status, else
        // `change_requests_submitted`; either stops after `limit` rows. The
        // repository id and two statements keep the planner on that walk,
        // with no join or optional filter to plan around.
        const ORDER: &str = "ORDER BY c.submitted_at DESC, c.id DESC";
        let from =
            format!("SELECT {CHANGE_REQUEST_COLUMNS} FROM change_requests c WHERE c.repo = $1");
        let repo = self.repo(repo).await?;
        let limit = i64::from(limit);
        let rows: Vec<ChangeRequestRow> = match status {
            Some(status) => {
                sqlx::query_as(&format!(
                    "{from} AND c.status = $2::change_request_status {ORDER} LIMIT $3"
                ))
                .bind(repo)
                .bind(status.as_str())
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(&format!("{from} {ORDER} LIMIT $2"))
                    .bind(repo)
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        rows.into_iter().map(change_request).collect()
    }

    async fn find_landings(
        &self,
        repo: &str,
        changesets: &[ChangesetId],
    ) -> Result<Vec<(ChangesetId, ChangeRequest)>> {
        let repo = self.repo(repo).await?;
        let ids: Vec<&[u8]> = changesets
            .iter()
            .map(|cs| cs.as_bytes().as_slice())
            .collect();
        let rows: Vec<ChangeRequestRow> = sqlx::query_as(&find_landings_sql())
            .bind(repo)
            .bind(ids)
            .fetch_all(&self.pool)
            .await?;
        let found = rows
            .into_iter()
            .map(change_request)
            .collect::<Result<Vec<_>>>()?;
        Ok(changesets
            .iter()
            .filter_map(|cs| {
                found
                    .iter()
                    .filter(|cr| cr.head == *cs || cr.landed == Some(*cs))
                    .min_by_key(|cr| cr.id)
                    .map(|cr| (*cs, cr.clone()))
            })
            .collect())
    }

    async fn claim_next_change_request(&self, repo: &str) -> Result<Option<ChangeRequest>> {
        // The oldest pending row through `change_requests_status`; a row
        // another claim holds is skipped, never waited on or returned twice.
        let row: Option<ChangeRequestRow> = sqlx::query_as(&format!(
            "UPDATE change_requests c SET status = 'landing' \
             WHERE (c.repo, c.id) = (SELECT p.repo, p.id FROM change_requests p \
             JOIN repos r ON r.id = p.repo WHERE r.name = $1 AND p.status = 'pending' \
             ORDER BY p.submitted_at, p.id LIMIT 1 FOR UPDATE OF p SKIP LOCKED) \
             RETURNING {CHANGE_REQUEST_COLUMNS}"
        ))
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;
        row.map(change_request).transpose()
    }

    async fn landing_change_request(&self, repo: &str) -> Result<Option<ChangeRequest>> {
        let row: Option<ChangeRequestRow> = sqlx::query_as(&format!(
            "SELECT {CHANGE_REQUEST_COLUMNS} FROM change_requests c JOIN repos r ON r.id = c.repo \
             WHERE r.name = $1 AND c.status = 'landing' ORDER BY c.submitted_at, c.id LIMIT 1"
        ))
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;
        row.map(change_request).transpose()
    }

    async fn finish_change_request(
        &self,
        repo: &str,
        id: u64,
        outcome: LandOutcome,
    ) -> Result<ChangeRequest> {
        let status = outcome.status();
        let (landed, conflicts) = match outcome {
            LandOutcome::Landed(cs) => (Some(cs.as_bytes().to_vec()), None),
            LandOutcome::NeedsRebase(paths) | LandOutcome::Denied(paths) => (None, Some(paths)),
            LandOutcome::Rejected => (None, None),
        };
        let row: Option<ChangeRequestRow> = sqlx::query_as(&format!(
            "UPDATE change_requests c SET status = $3::change_request_status, landed_cs = $4, \
             conflicts = $5, landed_at = CASE WHEN $4::bytea IS NULL THEN NULL ELSE now() END \
             FROM repos r WHERE r.id = c.repo AND r.name = $1 AND c.id = $2 \
             AND c.status = 'landing' RETURNING {CHANGE_REQUEST_COLUMNS}"
        ))
        .bind(repo)
        .bind(to_i64(id)?)
        .bind(status.as_str())
        .bind(landed)
        .bind(conflicts)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(row) = row {
            return change_request(row);
        }
        match self.get_change_request(repo, id).await? {
            None => Err(StoreError::ChangeRequestNotFound(id)),
            Some(cr) => Err(StoreError::ChangeRequestNotLanding {
                id,
                status: cr.status,
            }),
        }
    }

    async fn reject_change_request(&self, repo: &str, id: u64) -> Result<ChangeRequest> {
        let row: Option<ChangeRequestRow> = sqlx::query_as(&format!(
            "UPDATE change_requests c SET status = 'rejected', conflicts = NULL \
             FROM repos r WHERE r.id = c.repo AND r.name = $1 AND c.id = $2 \
             AND c.status IN ('pending', 'needs_rebase') RETURNING {CHANGE_REQUEST_COLUMNS}"
        ))
        .bind(repo)
        .bind(to_i64(id)?)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => change_request(row),
            None => self
                .get_change_request(repo, id)
                .await?
                .ok_or(StoreError::ChangeRequestNotFound(id)),
        }
    }

    async fn repos_with_open_change_requests(&self) -> Result<Vec<String>> {
        // Through `change_requests_status`, one probe per status.
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT r.name FROM repos r WHERE EXISTS (SELECT 1 FROM change_requests c \
             WHERE c.repo = r.id AND c.status IN ('pending', 'landing')) ORDER BY r.name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(name,)| name).collect())
    }

    async fn put_grant(&self, repo: &str, grant: &Grant) -> Result<()> {
        let repo_id = self.grant_repo(repo).await?;
        sqlx::query(
            "WITH i AS (INSERT INTO grants (repo, principal, path_prefix, role) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (repo, principal, path_prefix) \
             DO UPDATE SET role = EXCLUDED.role RETURNING 1) \
             SELECT pg_notify($5, $6)::text FROM i",
        )
        .bind(repo_id)
        .bind(&grant.principal)
        .bind(&grant.path_prefix)
        .bind(grant.role.as_str())
        .bind(INVALIDATE_CHANNEL)
        .bind(repo)
        .execute(&self.pool)
        .await?;
        self.mutable.forget(repo);
        Ok(())
    }

    async fn delete_grant(&self, repo: &str, principal: &str, path_prefix: &str) -> Result<bool> {
        let repo_id = self.grant_repo(repo).await?;
        let done: Option<(String,)> = sqlx::query_as(
            "WITH d AS (DELETE FROM grants WHERE repo = $1 AND principal = $2 \
             AND path_prefix = $3 RETURNING 1) \
             SELECT pg_notify($4, $5)::text FROM d",
        )
        .bind(repo_id)
        .bind(principal)
        .bind(path_prefix)
        .bind(INVALIDATE_CHANNEL)
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;
        self.mutable.forget(repo);
        Ok(done.is_some())
    }

    async fn grants_for(&self, repo: &str, principal: &str) -> Result<Vec<Grant>> {
        Ok(self
            .repo_grants(repo)
            .await?
            .iter()
            .filter(|g| g.principal == principal)
            .cloned()
            .collect())
    }

    async fn grants(&self, repo: &str) -> Result<Vec<Grant>> {
        Ok(self.repo_grants(repo).await?.to_vec())
    }

    async fn repos_of(&self, principal: &str) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT r.name COLLATE \"C\" FROM grants g JOIN repos r ON r.id = g.repo \
             WHERE g.principal = $1 AND r.name <> $2 ORDER BY 1",
        )
        .bind(principal)
        .bind(SERVER_GRANTS)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(name,)| name).collect())
    }
}

#[cfg(all(test, feature = "integration"))]
mod tests {
    use super::*;

    /// With 1,000 landed requests, `find_landings` plans as index probes
    /// only, under the generic plan a cached statement ends up with as well
    /// as a custom one.
    #[tokio::test]
    async fn find_landings_uses_the_indexes() {
        let opts: PgConnectOptions = std::env::var("SCION_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://scion:scion@localhost:5432/scion".to_owned())
            .parse()
            .unwrap();
        let name = format!("scion_test_{}_plan", std::process::id());
        let admin = PgPool::connect_with(opts.clone()).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&admin)
            .await
            .unwrap();
        let store = PostgresStore::connect_with(opts.database(&name))
            .await
            .unwrap();
        store.migrate().await.unwrap();
        store.create_repo("plan").await.unwrap();
        let cs = |i: u32, tag: u8| {
            let mut b = [tag; 32];
            b[..4].copy_from_slice(&i.to_be_bytes());
            ChangesetId::from_bytes(b)
        };
        for i in 0..1_000 {
            let cr = store
                .submit_change_request("plan", cs(i, 1), cs(0, 0), "sp:test")
                .await
                .unwrap();
            store.claim_next_change_request("plan").await.unwrap();
            store
                .finish_change_request("plan", cr.id, LandOutcome::Landed(cs(i, 2)))
                .await
                .unwrap();
        }
        let mut conn = store.pool().acquire().await.unwrap();
        sqlx::raw_sql("ANALYZE change_requests")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::raw_sql(&format!(
            "PREPARE find(bigint, bytea[]) AS {}",
            find_landings_sql()
        ))
        .execute(&mut *conn)
        .await
        .unwrap();
        let repo = store.repo("plan").await.unwrap();
        for mode in ["force_generic_plan", "force_custom_plan"] {
            sqlx::raw_sql(&format!("SET plan_cache_mode = {mode}"))
                .execute(&mut *conn)
                .await
                .unwrap();
            let plan: Vec<(String,)> = sqlx::query_as(&format!(
                "EXPLAIN EXECUTE find({repo}, ARRAY['\\x{}'::bytea, '\\x{}'::bytea])",
                cs(7, 1),
                cs(9, 2)
            ))
            .fetch_all(&mut *conn)
            .await
            .unwrap();
            let plan = plan
                .into_iter()
                .map(|(l,)| l)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                plan.contains("change_requests_landed_cs")
                    && plan.contains("change_requests_landed_head")
                    && !plan.contains("Seq Scan"),
                "{mode}:\n{plan}"
            );
        }
        drop(conn);
        store.pool().close().await;
        sqlx::query(&format!("DROP DATABASE \"{name}\" WITH (FORCE)"))
            .execute(&admin)
            .await
            .unwrap();
    }
}
