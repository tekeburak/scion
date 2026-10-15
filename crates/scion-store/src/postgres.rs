//! Postgres-backed [`MetadataStore`].
//!
//! Plain SQL over `sqlx` so that `CockroachDB` (Postgres wire protocol) remains a
//! drop-in target. The schema lives in `migrations/`; the canonical encoding of
//! every changeset is stored verbatim in `changesets.canonical`, so reads
//! reproduce the exact object that was hashed, and the structured columns
//! exist for querying.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use scion_types::{Changeset, ChangesetId, Decode, Encode, FileChange};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{Executor, Postgres};

use crate::recent::Recent;
use crate::{MetadataStore, RefState, Result, StoreError, is_allowed_ref_name};

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

    /// Look up a repository id, creating the repository if needed. A
    /// repository row is never deleted, so its id is memoised.
    async fn ensure_repo(&self, name: &str) -> Result<i64> {
        if let Some(id) = self.repo_ids.get(&name.to_owned()).await {
            return Ok(id);
        }
        sqlx::query("INSERT INTO repos (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
            .bind(name)
            .execute(&self.pool)
            .await?;
        let id = Self::repo_id(&self.pool, name)
            .await?
            .ok_or_else(|| StoreError::Corrupt(format!("repo {name:?} vanished after insert")))?;
        self.repo_ids.insert(name.to_owned(), id, name.len() + 8);
        Ok(id)
    }
}

/// Insert every file change of `cs` (stored as `id` in repository `repo`).
async fn insert_file_changes(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    repo: i64,
    id: ChangesetId,
    cs: &Changeset,
) -> Result<()> {
    // One statement for every file change: an imported root commit
    // touches thousands of paths, and a round trip each would dominate.
    let n = cs.file_changes.len();
    let (mut paths, mut kinds, mut blobs, mut ftypes, mut sizes, mut cf_paths, mut cf_css) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    for (path, change) in &cs.file_changes {
        paths.push(path.as_str());
        match change {
            FileChange::Deleted => {
                kinds.push(0i16);
                blobs.push(None);
                ftypes.push(None);
                sizes.push(None);
                cf_paths.push(None);
                cf_css.push(None);
            }
            FileChange::Tracked {
                content,
                kind,
                size,
                copy_from,
            } => {
                kinds.push(1i16);
                blobs.push(Some(content.as_bytes().to_vec()));
                ftypes.push(Some(i16::from(kind.tag())));
                sizes.push(Some(to_i64(*size)?));
                cf_paths.push(copy_from.as_ref().map(|(p, _)| p.as_str()));
                cf_css.push(copy_from.as_ref().map(|(_, c)| c.as_bytes().to_vec()));
            }
        }
    }
    if n > 0 {
        sqlx::query(
            "INSERT INTO file_changes (repo, changeset_id, path, change_kind, blob_id, file_type, \
             size, copy_from_path, copy_from_cs) \
             SELECT $1, $2, * FROM UNNEST($3::text[], $4::smallint[], $5::bytea[], \
             $6::smallint[], $7::bigint[], $8::text[], $9::bytea[])",
        )
        .bind(repo)
        .bind(id.as_bytes().as_slice())
        .bind(&paths)
        .bind(&kinds)
        .bind(&blobs)
        .bind(&ftypes)
        .bind(&sizes)
        .bind(&cf_paths)
        .bind(&cf_css)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

#[async_trait]
impl MetadataStore for PostgresStore {
    async fn put_changeset(&self, cs: &Changeset) -> Result<ChangesetId> {
        let id = cs.id();
        let canonical = cs.encode_to_vec();
        let repo = self.ensure_repo(&cs.repo).await?;
        let mut tx = self.pool.begin().await?;

        let exists: Option<(i32,)> =
            sqlx::query_as("SELECT 1 FROM changesets WHERE repo = $1 AND id = $2")
                .bind(repo)
                .bind(id.as_bytes().as_slice())
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_some() {
            tx.rollback().await?;
            return Ok(id);
        }

        // Generation = max(parent generation) + 1; every parent must exist.
        let parent_bytes: Vec<Vec<u8>> = cs.parents.iter().map(|p| p.as_bytes().to_vec()).collect();
        let rows: Vec<(Vec<u8>, i64)> = sqlx::query_as(
            "SELECT id, generation FROM changesets WHERE repo = $1 AND id = ANY($2)",
        )
        .bind(repo)
        .bind(&parent_bytes)
        .fetch_all(&mut *tx)
        .await?;
        let gens: HashMap<Vec<u8>, i64> = rows.into_iter().collect();
        let mut generation = 0i64;
        for p in &cs.parents {
            match gens.get(p.as_bytes().as_slice()) {
                Some(g) => generation = generation.max(g + 1),
                None => return Err(StoreError::UnknownParent(id, *p)),
            }
        }

        sqlx::query(
            "INSERT INTO changesets (repo, id, parents, first_parent, author_name, author_email, \
             author_date, author_tz, committer_name, committer_email, committer_date, committer_tz, \
             message, generation, canonical) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(repo)
        .bind(id.as_bytes().as_slice())
        .bind(&parent_bytes)
        .bind(cs.first_parent().map(|p| p.as_bytes().to_vec()))
        .bind(&cs.author.name)
        .bind(&cs.author.email)
        .bind(cs.author_date.unix_secs)
        .bind(cs.author_date.tz_offset_minutes)
        .bind(cs.committer.as_ref().map(|c| &c.name))
        .bind(cs.committer.as_ref().map(|c| &c.email))
        .bind(cs.committer_date.map(|t| t.unix_secs))
        .bind(cs.committer_date.map(|t| t.tz_offset_minutes))
        .bind(&cs.message)
        .bind(generation)
        .bind(&canonical)
        .execute(&mut *tx)
        .await?;

        insert_file_changes(&mut tx, repo, id, cs).await?;
        tx.commit().await?;
        self.changesets
            .insert((cs.repo.clone(), id), cs.clone(), canonical.len());
        Ok(id)
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
        let repo_id = self.ensure_repo(repo).await?;
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
            None => Err(StoreError::RefNotFound(name.to_owned())),
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

    async fn put_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
        pointer: Vec<u8>,
    ) -> Result<()> {
        let repo_id = self.ensure_repo(repo).await?;
        sqlx::query(
            "INSERT INTO derived (repo, type, version, changeset_id, pointer) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(repo_id)
        .bind(ty)
        .bind(
            i32::try_from(version)
                .map_err(|_| StoreError::Corrupt("derived version overflow".into()))?,
        )
        .bind(cs.as_bytes().as_slice())
        .bind(&pointer)
        .execute(&self.pool)
        .await?;
        // A conflicting insert kept the first pointer; both are the same
        // value of a deterministic derivation, so either may be memoised.
        let weight = pointer.len() + ty.len() + repo.len() + 40;
        self.derived.insert(
            (repo.to_owned(), ty.to_owned(), version, cs),
            pointer,
            weight,
        );
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

    async fn put_ingest_checkpoint(
        &self,
        repo: &str,
        git_sha: &[u8],
        cs: ChangesetId,
    ) -> Result<()> {
        let repo_id = self.ensure_repo(repo).await?;
        sqlx::query(
            "INSERT INTO ingest_checkpoints (repo, git_sha, changeset_id) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(repo_id)
        .bind(git_sha)
        .bind(cs.as_bytes().as_slice())
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
}
