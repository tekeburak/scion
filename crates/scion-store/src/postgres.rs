//! Postgres-backed [`MetadataStore`].
//!
//! Plain SQL over `sqlx` so that `CockroachDB` (Postgres wire protocol) remains a
//! drop-in target. The schema lives in `migrations/`; the canonical encoding of
//! every changeset is stored verbatim in `changesets.canonical`, so reads
//! reproduce the exact object that was hashed, and the structured columns
//! exist for querying.

use std::collections::HashMap;

use async_trait::async_trait;
use scion_types::{Changeset, ChangesetId, Decode, Encode, FileChange};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{Executor, Postgres, Transaction};

use crate::{MetadataStore, RefState, Result, StoreError, is_allowed_ref_name};

/// Embedded migrations from the repository's `migrations/` directory.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// A [`MetadataStore`] backed by Postgres.
#[derive(Debug, Clone)]
pub struct PostgresStore {
    pool: PgPool,
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
        Ok(Self { pool })
    }

    /// Wrap an existing pool.
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
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

    /// Look up a repository id, creating the repository if needed.
    async fn ensure_repo(tx: &mut Transaction<'_, Postgres>, name: &str) -> Result<i64> {
        sqlx::query("INSERT INTO repos (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
            .bind(name)
            .execute(&mut **tx)
            .await?;
        Self::repo_id(&mut **tx, name)
            .await?
            .ok_or_else(|| StoreError::Corrupt(format!("repo {name:?} vanished after insert")))
    }
}

#[async_trait]
impl MetadataStore for PostgresStore {
    async fn put_changeset(&self, cs: &Changeset) -> Result<ChangesetId> {
        let id = cs.id();
        let canonical = cs.encode_to_vec();
        let mut tx = self.pool.begin().await?;
        let repo = Self::ensure_repo(&mut tx, &cs.repo).await?;

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

        // TODO(perf): batch with UNNEST once changesets routinely touch many files.
        for (path, change) in &cs.file_changes {
            let (kind, blob, ftype, size, cf_path, cf_cs) = match change {
                FileChange::Deleted => (0i16, None, None, None, None, None),
                FileChange::Tracked {
                    content,
                    kind,
                    size,
                    copy_from,
                } => (
                    1i16,
                    Some(content.as_bytes().to_vec()),
                    Some(i16::from(kind.tag())),
                    Some(to_i64(*size)?),
                    copy_from.as_ref().map(|(p, _)| p.as_str().to_owned()),
                    copy_from.as_ref().map(|(_, c)| c.as_bytes().to_vec()),
                ),
            };
            sqlx::query(
                "INSERT INTO file_changes (repo, changeset_id, path, change_kind, blob_id, file_type, \
                 size, copy_from_path, copy_from_cs) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(repo)
            .bind(id.as_bytes().as_slice())
            .bind(path.as_str())
            .bind(kind)
            .bind(blob)
            .bind(ftype)
            .bind(size)
            .bind(cf_path)
            .bind(cf_cs)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(id)
    }

    async fn get_changeset(&self, repo: &str, id: ChangesetId) -> Result<Option<Changeset>> {
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
        let mut tx = self.pool.begin().await?;
        let repo_id = Self::ensure_repo(&mut tx, repo).await?;
        let inserted = sqlx::query(
            "INSERT INTO refs (repo, name, target, version) VALUES ($1, $2, $3, 0) \
             ON CONFLICT (repo, name) DO NOTHING",
        )
        .bind(repo_id)
        .bind(name)
        .bind(target.as_bytes().as_slice())
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            tx.rollback().await?;
            return Err(StoreError::RefExists(name.to_owned()));
        }
        tx.commit().await?;
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
        let mut tx = self.pool.begin().await?;
        let repo_id = Self::ensure_repo(&mut tx, repo).await?;
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
        .bind(pointer)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn get_derived(
        &self,
        repo: &str,
        ty: &str,
        version: u32,
        cs: ChangesetId,
    ) -> Result<Option<Vec<u8>>> {
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
        Ok(row.map(|(p,)| p))
    }

    async fn put_ingest_checkpoint(
        &self,
        repo: &str,
        git_sha: &[u8],
        cs: ChangesetId,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let repo_id = Self::ensure_repo(&mut tx, repo).await?;
        sqlx::query(
            "INSERT INTO ingest_checkpoints (repo, git_sha, changeset_id) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(repo_id)
        .bind(git_sha)
        .bind(cs.as_bytes().as_slice())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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
