use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, TransactionBehavior};

use crate::{timestamp_millis, JobRecord};

const SCHEMA_VERSION: i64 = 1;

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
                    PRAGMA user_version = 1;",
                )?;
                transaction.commit()?;
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
}
