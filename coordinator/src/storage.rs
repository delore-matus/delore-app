use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::{timestamp_millis, JobRecord};

const SCHEMA_VERSION: i64 = 4;

#[derive(serde::Serialize)]
struct PendingPayout {
    job_id: String,
    provider_id: String,
    wallet_address: String,
    amount: u64,
}

#[derive(Clone)]
pub(super) struct JobStore {
    connection: Arc<Mutex<Connection>>,
}

impl JobStore {
    pub(super) fn open(path: &Path) -> Result<Self> {
        if path.as_os_str().is_empty() {
            bail!("coordinator database path cannot be empty");
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
        }

        let connection = Connection::open(path)
            .with_context(|| format!("failed to open coordinator database {}", path.display()))?;
        Self::initialize(connection, true)
    }

    #[cfg(test)]
    pub(super) fn in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?, false)
    }

    fn initialize(mut connection: Connection, use_wal: bool) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        if use_wal {
            connection
                .pragma_update(None, "journal_mode", "WAL")
                .context("failed to enable SQLite write-ahead logging")?;
        }
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "FULL")?;

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                transaction.execute_batch(
                    "CREATE TABLE jobs (
                        job_id TEXT PRIMARY KEY NOT NULL,
                        status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'completed', 'failed')),
                        job_json TEXT NOT NULL,
                        created_at INTEGER NOT NULL,
                        updated_at INTEGER NOT NULL
                    );
                    CREATE INDEX jobs_status_created_at ON jobs(status, created_at, job_id);
                    CREATE TABLE providers (
                        provider_id TEXT PRIMARY KEY NOT NULL,
                        wallet_address TEXT NOT NULL,
                        auth_token_hash TEXT,
                        total_earned INTEGER NOT NULL DEFAULT 0,
                        pending_payout INTEGER NOT NULL DEFAULT 0,
                        tx_hash TEXT
                    );
                    CREATE TABLE payouts (
                        provider_id TEXT NOT NULL,
                        amount INTEGER NOT NULL,
                        tx_hash TEXT,
                        settled_at INTEGER,
                        FOREIGN KEY(provider_id) REFERENCES providers(provider_id)
                    );
                    CREATE TABLE pending_payouts (
                        job_id TEXT PRIMARY KEY NOT NULL,
                        provider_id TEXT NOT NULL,
                        wallet_address TEXT NOT NULL,
                        amount INTEGER NOT NULL CHECK (amount >= 0),
                        FOREIGN KEY(provider_id) REFERENCES providers(provider_id)
                    );
                    PRAGMA user_version = 4;",
                )?;
                transaction.commit()?;
            }
            1 => {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                transaction.execute_batch(
                    "CREATE TABLE providers (
                        provider_id TEXT PRIMARY KEY NOT NULL,
                        wallet_address TEXT NOT NULL,
                        auth_token_hash TEXT,
                        total_earned INTEGER NOT NULL DEFAULT 0,
                        pending_payout INTEGER NOT NULL DEFAULT 0,
                        tx_hash TEXT
                    );
                    CREATE TABLE payouts (
                        provider_id TEXT NOT NULL,
                        amount INTEGER NOT NULL,
                        tx_hash TEXT,
                        settled_at INTEGER,
                        FOREIGN KEY(provider_id) REFERENCES providers(provider_id)
                    );
                    CREATE TABLE pending_payouts (
                        job_id TEXT PRIMARY KEY NOT NULL,
                        provider_id TEXT NOT NULL,
                        wallet_address TEXT NOT NULL,
                        amount INTEGER NOT NULL CHECK (amount >= 0),
                        FOREIGN KEY(provider_id) REFERENCES providers(provider_id)
                    );
                    PRAGMA user_version = 4;",
                )?;
                transaction.commit()?;
            }
            2 => {
                connection.execute_batch(
                    "CREATE TABLE pending_payouts (
                        job_id TEXT PRIMARY KEY NOT NULL,
                        provider_id TEXT NOT NULL,
                        wallet_address TEXT NOT NULL,
                        amount INTEGER NOT NULL CHECK (amount >= 0),
                        FOREIGN KEY(provider_id) REFERENCES providers(provider_id)
                    );
                    ALTER TABLE providers ADD COLUMN auth_token_hash TEXT;
                    PRAGMA user_version = 4;",
                )?;
            }
            3 => {
                connection.execute_batch(
                    "ALTER TABLE providers ADD COLUMN auth_token_hash TEXT;
                    PRAGMA user_version = 4;",
                )?;
            }
            SCHEMA_VERSION => {}
            other => bail!("unsupported coordinator database schema version {other}"),
        }

        let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            bail!("coordinator database integrity check failed: {integrity}");
        }

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub(super) fn load_and_recover(&self) -> Result<Vec<JobRecord>> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("SQLite connection mutex was poisoned"))?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let serialized_jobs = {
            let mut statement =
                transaction.prepare("SELECT job_json FROM jobs ORDER BY created_at, rowid")?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut jobs = serialized_jobs
            .iter()
            .map(|job| serde_json::from_str::<JobRecord>(job))
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to decode a persisted coordinator job")?;

        for job in &mut jobs {
            if job.status == "running" {
                job.status = "failed".to_string();
                job.error = Some("Coordinator restarted while job was running".to_string());
                write_job(&transaction, job)?;
            }
        }

        transaction.commit()?;
        Ok(jobs)
    }

    pub(super) async fn save_job(&self, job: JobRecord) -> Result<()> {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || {
            let connection = connection
                .lock()
                .map_err(|_| anyhow!("SQLite connection mutex was poisoned"))?;
            write_job(&connection, &job)
        })
        .await
        .context("SQLite job writer task failed")?
    }

    pub(super) async fn register_provider(
        &self,
        provider_id: &str,
        wallet_address: &str,
        auth_token_hash: Option<&str>,
    ) -> Result<bool> {
        let connection = Arc::clone(&self.connection);
        let provider_id = provider_id.to_string();
        let wallet_address = wallet_address.to_string();
        let auth_token_hash = auth_token_hash.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| anyhow!("SQLite connection mutex was poisoned"))?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let known_hash = transaction
                .query_row(
                    "SELECT auth_token_hash FROM providers WHERE provider_id=?1",
                    params![provider_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            if known_hash
                .as_deref()
                .is_some_and(|known| auth_token_hash.as_deref() != Some(known))
            {
                transaction.rollback()?;
                return Ok(false);
            }
            transaction.execute(
                "INSERT INTO providers (provider_id, wallet_address, auth_token_hash)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(provider_id) DO UPDATE SET
                    wallet_address=excluded.wallet_address,
                    auth_token_hash=COALESCE(providers.auth_token_hash, excluded.auth_token_hash)",
                params![provider_id, wallet_address, auth_token_hash],
            )?;
            transaction.commit()?;
            Ok(true)
        })
        .await
        .context("SQLite provider writer task failed")?
    }

    pub(super) async fn add_pending_payout(
        &self,
        provider_id: &str,
        wallet_address: &str,
        job_id: &str,
        amount: u64,
    ) -> Result<()> {
        let connection = Arc::clone(&self.connection);
        let provider_id = provider_id.to_string();
        let wallet_address = wallet_address.to_string();
        let job_id = job_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| anyhow!("SQLite connection mutex was poisoned"))?;
            let amount = i64::try_from(amount).context("pending payout exceeds SQLite range")?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let inserted = transaction.execute(
                "INSERT OR IGNORE INTO pending_payouts
                    (job_id, provider_id, wallet_address, amount)
                 VALUES (?1, ?2, ?3, ?4)",
                params![job_id, provider_id, wallet_address, amount],
            )?;
            if inserted != 0 {
                transaction.execute(
                    "INSERT INTO providers
                        (provider_id, wallet_address, total_earned, pending_payout)
                     VALUES (?1, ?2, ?3, ?3)
                     ON CONFLICT(provider_id) DO UPDATE SET
                       wallet_address=excluded.wallet_address,
                       total_earned=(
                         SELECT COALESCE(SUM(amount), 0)
                         FROM pending_payouts
                         WHERE provider_id=excluded.provider_id
                       ),
                       pending_payout=providers.pending_payout+excluded.pending_payout",
                    params![provider_id, wallet_address, amount],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
        .context("SQLite payout writer task failed")?
    }

    pub(super) async fn provider_earnings(&self, provider_id: &str) -> Result<(u64, u64)> {
        let connection = Arc::clone(&self.connection);
        let provider_id = provider_id.to_string();
        tokio::task::spawn_blocking(move || {
            let connection = connection
                .lock()
                .map_err(|_| anyhow!("SQLite connection mutex was poisoned"))?;
            let (jobs_completed, total_earned): (i64, i64) = connection.query_row(
                "SELECT COUNT(*), COALESCE(SUM(amount), 0)
                 FROM pending_payouts WHERE provider_id=?1",
                params![provider_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            Ok((
                u64::try_from(jobs_completed).context("provider job count is negative")?,
                u64::try_from(total_earned).context("provider earnings are negative")?,
            ))
        })
        .await
        .context("SQLite provider earnings reader task failed")?
    }

    pub(super) fn export_pending_payouts(&self, output_path: &Path) -> Result<usize> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("SQLite connection mutex was poisoned"))?;
        let mut statement = connection.prepare(
            "SELECT job_id, provider_id, wallet_address, amount
             FROM pending_payouts ORDER BY job_id",
        )?;
        let payouts = statement
            .query_map([], |row| {
                let amount: i64 = row.get(3)?;
                Ok(PendingPayout {
                    job_id: row.get(0)?,
                    provider_id: row.get(1)?,
                    wallet_address: row.get(2)?,
                    amount: u64::try_from(amount).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Integer,
                            Box::new(error),
                        )
                    })?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let serialized =
            serde_json::to_vec_pretty(&payouts).context("failed to encode pending payouts")?;
        if let Some(parent) = output_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(output_path, serialized)
            .with_context(|| format!("failed to write {}", output_path.display()))?;
        Ok(payouts.len())
    }

    #[cfg(test)]
    pub(super) async fn pending_payout(&self, provider_id: &str) -> Result<u64> {
        let connection = Arc::clone(&self.connection);
        let provider_id = provider_id.to_string();
        tokio::task::spawn_blocking(move || {
            let connection = connection
                .lock()
                .map_err(|_| anyhow!("SQLite connection mutex was poisoned"))?;
            let value: i64 = connection.query_row(
                "SELECT pending_payout FROM providers WHERE provider_id=?1",
                params![provider_id],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(value).unwrap_or(0))
        })
        .await
        .context("SQLite payout reader task failed")?
    }
}

fn write_job(connection: &Connection, job: &JobRecord) -> Result<()> {
    let job_json = serde_json::to_string(job).context("failed to encode coordinator job")?;
    let now = timestamp_millis().min(i64::MAX as u128) as i64;
    connection.execute(
        "INSERT INTO jobs (job_id, status, job_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(job_id) DO UPDATE SET
             status = excluded.status,
             job_json = excluded.job_json,
             updated_at = excluded.updated_at",
        params![job.id, job.status, job_json, now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use common::JobResult;

    use crate::{FeeBreakdown, JobRecord};

    use super::JobStore;

    static NEXT_TEST_DATABASE: AtomicU64 = AtomicU64::new(1);

    fn database_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "gpuswarm-coordinator-{}-{}.sqlite3",
            std::process::id(),
            NEXT_TEST_DATABASE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn job(id: &str, status: &str) -> JobRecord {
        JobRecord {
            id: id.to_string(),
            prompt: format!("prompt for {id}"),
            status: status.to_string(),
            provider_id: (status != "queued").then(|| "provider-1".to_string()),
            result: None,
            fee_breakdown: None,
            error: None,
        }
    }

    fn remove_database(path: &PathBuf) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn persists_jobs_and_fails_interrupted_work_after_restart() {
        let path = database_path();
        let store = JobStore::open(&path).unwrap();

        store.save_job(job("queued-job", "queued")).await.unwrap();
        store.save_job(job("running-job", "running")).await.unwrap();

        let mut completed = job("completed-job", "completed");
        completed.result = Some(JobResult {
            job_id: completed.id.clone(),
            provider_id: "provider-1".to_string(),
            result_data: "completed".to_string(),
            duration_ms: 123,
            eval_count: 7,
            tokens_per_sec: 4.5,
            error: None,
        });
        completed.fee_breakdown = Some(FeeBreakdown {
            gross_price: 10_000,
            platform_fee: 1_000,
            provider_payout: 9_000,
        });
        store.save_job(completed).await.unwrap();
        drop(store);

        let reopened = JobStore::open(&path).unwrap();
        let jobs = reopened.load_and_recover().unwrap();
        let by_id = jobs
            .into_iter()
            .map(|job| (job.id.clone(), job))
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(by_id["queued-job"].status, "queued");
        assert_eq!(by_id["completed-job"].status, "completed");
        assert_eq!(
            by_id["completed-job"]
                .fee_breakdown
                .as_ref()
                .unwrap()
                .provider_payout,
            9_000
        );
        assert_eq!(by_id["running-job"].status, "failed");
        assert_eq!(
            by_id["running-job"].error.as_deref(),
            Some("Coordinator restarted while job was running")
        );

        drop(reopened);
        remove_database(&path);
    }

    #[tokio::test]
    async fn provider_wallet_and_pending_payout_are_persisted_once() {
        let path = database_path();
        let store = JobStore::open(&path).unwrap();
        let wallet = "0x1111111111111111111111111111111111111111";

        assert!(store
            .register_provider("provider-1", wallet, None)
            .await
            .unwrap());
        store
            .add_pending_payout("provider-1", wallet, "job-1", 11_700)
            .await
            .unwrap();
        store
            .add_pending_payout("provider-1", wallet, "job-1", 11_700)
            .await
            .unwrap();

        let pending = store.pending_payout("provider-1").await.unwrap();
        assert_eq!(pending, 11_700);
        assert_eq!(
            store.provider_earnings("provider-1").await.unwrap(),
            (1, 11_700)
        );
        let total_earned: i64 = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT total_earned FROM providers WHERE provider_id=?1",
                rusqlite::params!["provider-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(total_earned, 11_700);

        let export_path = database_path().with_extension("json");
        assert_eq!(store.export_pending_payouts(&export_path).unwrap(), 1);
        let exported: serde_json::Value =
            serde_json::from_slice(&fs::read(&export_path).unwrap()).unwrap();
        assert_eq!(exported[0]["job_id"], "job-1");
        assert_eq!(exported[0]["provider_id"], "provider-1");
        assert_eq!(exported[0]["wallet_address"], wallet);
        assert_eq!(exported[0]["amount"], 11_700);

        drop(store);
        let reopened = JobStore::open(&path).unwrap();
        assert_eq!(
            reopened.provider_earnings("provider-1").await.unwrap(),
            (1, 11_700)
        );
        drop(reopened);
        remove_database(&path);
        let _ = fs::remove_file(export_path);
    }

    #[tokio::test]
    async fn provider_token_hash_is_persisted_and_mismatched_tokens_are_rejected() {
        let path = database_path();
        let store = JobStore::open(&path).unwrap();
        let wallet = "0x1111111111111111111111111111111111111111";
        let secret = "provider-secret";
        let expected_hash = crate::hash_provider_secret(secret);
        let wrong_hash = crate::hash_provider_secret("different-secret");

        assert!(store
            .register_provider("provider-1", wallet, Some(&expected_hash))
            .await
            .unwrap());
        drop(store);

        let store = JobStore::open(&path).unwrap();
        assert!(!store
            .register_provider("provider-1", wallet, Some(&wrong_hash))
            .await
            .unwrap());
        assert!(!store
            .register_provider("provider-1", wallet, None)
            .await
            .unwrap());
        assert!(store
            .register_provider("provider-1", wallet, Some(&expected_hash))
            .await
            .unwrap());

        let stored_hash: String = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT auth_token_hash FROM providers WHERE provider_id=?1",
                rusqlite::params!["provider-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_hash, expected_hash);
        assert_ne!(stored_hash, secret);

        drop(store);
        remove_database(&path);
    }
}
