//! Postgres-backed [`MetadataStore`].
//!
//! Plain SQL over `sqlx` so that `CockroachDB` (Postgres wire protocol) remains a
//! drop-in target. The schema lives in `migrations/`; the canonical encoding of
//! every changeset is stored verbatim in `changesets.canonical`, so reads
//! reproduce the exact object that was hashed, and the structured columns
//! exist for querying.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use scion_types::{Changeset, ChangesetId, Decode, Encode, FileChange};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{Executor, Postgres};

use crate::recent::Recent;
use crate::{
    ChangeRequest, ChangeRequestStatus, DerivedPointer, LandOutcome, MetadataStore, RefState,
    Result, StoreError, is_allowed_ref_name,
};

/// Embedded migrations from the repository's `migrations/` directory.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Bytes of recently read or written changesets kept in memory.
const CHANGESET_MEMO_BYTES: usize = 32 << 20;
/// Bytes of recently read or written derived pointers kept in memory.
const DERIVED_MEMO_BYTES: usize = 16 << 20;
/// Bytes of repository ids kept in memory.
const REPO_MEMO_BYTES: usize = 1 << 20;

type DerivedKey = (String, String, u32, ChangesetId);

/// A [`MetadataStore`] backed by Postgres.
///
/// Changesets, derived pointers and repository ids are write-once, so the
/// recently read or written ones are memoised in a bounded amount of memory;
/// refs are always read from the database. Clones share the memo.
#[derive(Debug, Clone)]
pub struct PostgresStore {
    pool: PgPool,
    repo_ids: Arc<Recent<String, i64>>,
    changesets: Arc<Recent<(String, ChangesetId), Changeset>>,
    derived: Arc<Recent<DerivedKey, Vec<u8>>>,
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
        }
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
const CHANGE_REQUEST_COLUMNS: &str = "c.id, c.head_cs, c.base_cs, c.status::text, c.landed_cs, \
     c.conflicts, (EXTRACT(EPOCH FROM c.submitted_at) * 1000000)::bigint, \
     (EXTRACT(EPOCH FROM c.landed_at) * 1000000)::bigint";

type ChangeRequestRow = (
    i64,
    Vec<u8>,
    Vec<u8>,
    String,
    Option<Vec<u8>>,
    Option<Vec<String>>,
    i64,
    Option<i64>,
);

fn change_request(
    (id, head, base, status, landed, conflicts, submitted_us, landed_us): ChangeRequestRow,
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

#[async_trait]
impl MetadataStore for PostgresStore {
    async fn create_repo(&self, name: &str) -> Result<()> {
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
        for (canonical,) in rows {
            let cs = Changeset::decode(&canonical)
                .map_err(|e| StoreError::Corrupt(format!("changeset: {e}")))?;
            let id = cs.id();
            self.changesets
                .insert((repo.to_owned(), id), cs.clone(), canonical.len());
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
        let inserted = sqlx::query(
            "INSERT INTO refs (repo, name, target, version) VALUES ($1, $2, $3, 0) \
             ON CONFLICT (repo, name) DO NOTHING",
        )
        .bind(repo_id)
        .bind(name)
        .bind(target.as_bytes().as_slice())
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() == 0 {
            return Err(StoreError::RefExists(name.to_owned()));
        }
        Ok(RefState { target, version: 0 })
    }

    async fn get_ref(&self, repo: &str, name: &str) -> Result<Option<RefState>> {
        let row: Option<(Vec<u8>, i64)> = sqlx::query_as(
            "SELECT f.target, f.version FROM refs f JOIN repos r ON r.id = f.repo \
             WHERE r.name = $1 AND f.name = $2",
        )
        .bind(repo)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(t, v)| {
            Ok(RefState {
                target: ChangesetId::try_from_slice(&t)
                    .map_err(|e| StoreError::Corrupt(e.to_string()))?,
                version: to_u64(v)?,
            })
        })
        .transpose()
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
        let updated: Option<(i64,)> = sqlx::query_as(
            "UPDATE refs f SET target = $3, version = f.version + 1, updated_at = now() \
             FROM repos r WHERE r.id = f.repo AND r.name = $1 AND f.name = $2 AND f.version = $4 \
             RETURNING f.version",
        )
        .bind(repo)
        .bind(name)
        .bind(new_target.as_bytes().as_slice())
        .bind(expected)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((version,)) = updated {
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
    ) -> Result<ChangeRequest> {
        let repo_id = self.repo(repo).await?;
        let row: ChangeRequestRow = sqlx::query_as(&format!(
            "INSERT INTO change_requests AS c (repo, head_cs, base_cs) VALUES ($1, $2, $3) \
             RETURNING {CHANGE_REQUEST_COLUMNS}"
        ))
        .bind(repo_id)
        .bind(head.as_bytes().as_slice())
        .bind(base.as_bytes().as_slice())
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
            LandOutcome::NeedsRebase(paths) => (None, Some(paths)),
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
                .submit_change_request("plan", cs(i, 1), cs(0, 0))
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
