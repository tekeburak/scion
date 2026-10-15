//! Postgres-backed [`MetadataStore`].
//!
//! Plain SQL over `sqlx` so that `CockroachDB` (Postgres wire protocol) remains a
//! drop-in target. The schema lives in `migrations/`; the canonical encoding of
//! every changeset is stored verbatim in `changesets.canonical`, so reads
//! reproduce the exact object that was hashed, and the structured columns
//! exist for querying.

use std::collections::HashSet;

use async_trait::async_trait;
use scion_types::{Changeset, ChangesetId, Decode, Encode, FileChange};
use sqlx::postgres::{PgArguments, PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::query::Query;
use sqlx::{Executor, PgConnection, Postgres};

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

    /// Look up a repository id, creating the repository if needed. The
    /// repository almost always exists, so that case is one round trip.
    async fn ensure_repo(conn: &mut PgConnection, name: &str) -> Result<i64> {
        if let Some(id) = Self::repo_id(&mut *conn, name).await? {
            return Ok(id);
        }
        sqlx::query("INSERT INTO repos (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
            .bind(name)
            .execute(&mut *conn)
            .await?;
        Self::repo_id(&mut *conn, name)
            .await?
            .ok_or_else(|| StoreError::Corrupt(format!("repo {name:?} vanished after insert")))
    }

    /// Run `insert`, an `INSERT … SELECT … FROM repos WHERE name = $1`, in
    /// one round trip when the repository exists; otherwise create it and
    /// run `insert` again.
    async fn insert_into_repo<'q>(
        &self,
        repo: &str,
        insert: impl Fn() -> Query<'q, Postgres, PgArguments>,
    ) -> Result<()> {
        if insert().execute(&self.pool).await?.rows_affected() == 0
            && Self::repo_id(&self.pool, repo).await?.is_none()
        {
            Self::ensure_repo(&mut *self.pool.acquire().await?, repo).await?;
            insert().execute(&self.pool).await?;
        }
        Ok(())
    }
}

/// `file_changes` rows of one changeset as columns, for one `UNNEST` insert.
struct FileChangeColumns<'a> {
    paths: Vec<&'a str>,
    kinds: Vec<i16>,
    blobs: Vec<Option<Vec<u8>>>,
    ftypes: Vec<Option<i16>>,
    sizes: Vec<Option<i64>>,
    cf_paths: Vec<Option<&'a str>>,
    cf_css: Vec<Option<Vec<u8>>>,
}

impl<'a> FileChangeColumns<'a> {
    fn new(cs: &'a Changeset) -> Result<Self> {
        let n = cs.file_changes.len();
        let mut paths = Vec::with_capacity(n);
        let mut kinds = Vec::with_capacity(n);
        let mut blobs = Vec::with_capacity(n);
        let mut ftypes = Vec::with_capacity(n);
        let mut sizes = Vec::with_capacity(n);
        let mut cf_paths = Vec::with_capacity(n);
        let mut cf_css = Vec::with_capacity(n);
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

        Ok(Self {
            paths,
            kinds,
            blobs,
            ftypes,
            sizes,
            cf_paths,
            cf_css,
        })
    }
}

#[async_trait]
impl MetadataStore for PostgresStore {
    async fn put_changeset(&self, cs: &Changeset) -> Result<ChangesetId> {
        let id = cs.id();
        let canonical = cs.encode_to_vec();
        let parents: Vec<Vec<u8>> = cs.parents.iter().map(|p| p.as_bytes().to_vec()).collect();
        let distinct_parents = cs.parents.iter().collect::<HashSet<_>>().len();
        let distinct_parents = i64::try_from(distinct_parents).expect("parent count fits");

        let fc = FileChangeColumns::new(cs)?;

        // One statement, so one round trip: the changeset row (generation =
        // max(parent generation) + 1, inserted only if every parent exists)
        // and its file changes. It reports whether the repository and all
        // parents were found and whether the row is new.
        let insert = || {
            sqlx::query_as::<_, (i64, i64, i64)>(
                "WITH r AS (SELECT id FROM repos WHERE name = $1), \
                 par AS (SELECT c.generation FROM changesets c JOIN r ON c.repo = r.id \
                         WHERE c.id = ANY($3)), \
                 ins AS (INSERT INTO changesets (repo, id, parents, first_parent, author_name, \
                         author_email, author_date, author_tz, committer_name, committer_email, \
                         committer_date, committer_tz, message, generation, canonical) \
                         SELECT r.id, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, \
                         COALESCE((SELECT max(generation) + 1 FROM par), 0), $14 FROM r \
                         WHERE (SELECT count(*) FROM par) = $15 \
                         ON CONFLICT (repo, id) DO NOTHING RETURNING repo), \
                 fc AS (INSERT INTO file_changes (repo, changeset_id, path, change_kind, blob_id, \
                        file_type, size, copy_from_path, copy_from_cs) \
                        SELECT ins.repo, $2, u.* FROM ins, UNNEST($16::text[], $17::int2[], \
                        $18::bytea[], $19::int2[], $20::int8[], $21::text[], $22::bytea[]) AS u) \
                 SELECT (SELECT count(*) FROM r), (SELECT count(*) FROM par), \
                        (SELECT count(*) FROM ins)",
            )
            .bind(&cs.repo)
            .bind(id.as_bytes().as_slice())
            .bind(&parents)
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
            .bind(&canonical)
            .bind(distinct_parents)
            .bind(&fc.paths)
            .bind(&fc.kinds)
            .bind(&fc.blobs)
            .bind(&fc.ftypes)
            .bind(&fc.sizes)
            .bind(&fc.cf_paths)
            .bind(&fc.cf_css)
        };

        let mut found = insert().fetch_one(&self.pool).await?;
        if found.0 == 0 {
            // First changeset of a new repository.
            Self::ensure_repo(&mut *self.pool.acquire().await?, &cs.repo).await?;
            found = insert().fetch_one(&self.pool).await?;
        }
        let (_, parents_found, _) = found;
        if parents_found < distinct_parents {
            let present: Vec<(Vec<u8>,)> = sqlx::query_as(
                "SELECT c.id FROM changesets c JOIN repos r ON r.id = c.repo \
                 WHERE r.name = $1 AND c.id = ANY($2)",
            )
            .bind(&cs.repo)
            .bind(&parents)
            .fetch_all(&self.pool)
            .await?;
            let present: HashSet<Vec<u8>> = present.into_iter().map(|(p,)| p).collect();
            let missing = cs
                .parents
                .iter()
                .find(|p| !present.contains(p.as_bytes().as_slice()))
                .copied()
                .unwrap_or(cs.parents[0]);
            return Err(StoreError::UnknownParent(id, missing));
        }
        // Inserted, or already stored: either way the changeset is there.
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
        let version = i32::try_from(version)
            .map_err(|_| StoreError::Corrupt("derived version overflow".into()))?;
        self.insert_into_repo(repo, || {
            sqlx::query(
                "INSERT INTO derived (repo, type, version, changeset_id, pointer) \
                 SELECT id, $2, $3, $4, $5 FROM repos WHERE name = $1 ON CONFLICT DO NOTHING",
            )
            .bind(repo)
            .bind(ty)
            .bind(version)
            .bind(cs.as_bytes().as_slice())
            .bind(pointer.as_slice())
        })
        .await
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
        self.insert_into_repo(repo, || {
            sqlx::query(
                "INSERT INTO ingest_checkpoints (repo, git_sha, changeset_id) \
                 SELECT id, $2, $3 FROM repos WHERE name = $1 ON CONFLICT DO NOTHING",
            )
            .bind(repo)
            .bind(git_sha)
            .bind(cs.as_bytes().as_slice())
        })
        .await
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
