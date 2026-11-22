use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use provider_core::FeeConfig;
use serde::{Deserialize, Serialize};

const MAX_HISTORY_BYTES: usize = 1024 * 1024;
const MAX_HISTORY_ENTRIES: usize = 5000;
const MAX_JOB_ID_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EarningsStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EarningsEntry {
    pub job_id: String,
    pub timestamp_unix_ms: u128,
    pub status: EarningsStatus,
    pub gross_units: Option<u64>,
    pub provider_payout_units: Option<u64>,
    pub fee_units: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EarningsSnapshot {
    pub entries: Vec<EarningsEntry>,
    pub balance_units: u64,
    pub job_count: usize,
    pub recovered_corruption: bool,
}

pub struct EarningsStore {
    path: PathBuf,
}

impl EarningsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<EarningsSnapshot> {
        let (entries, recovered_corruption) = self.read_entries()?;
        let entries = if self.needs_rotation(&entries)? {
            self.persist(entries)?
        } else {
            entries
        };
        Ok(snapshot(entries, recovered_corruption))
    }

    pub fn record_result(
        &self,
        job_id: &str,
        succeeded: bool,
        fees: Option<&FeeConfig>,
    ) -> Result<EarningsSnapshot> {
        if job_id.is_empty() || job_id.len() > MAX_JOB_ID_BYTES {
            bail!("job id length is outside the history-store limit");
        }
        let (mut entries, recovered_corruption) = self.read_entries()?;
        let (status, gross_units, provider_payout_units, fee_units) = if succeeded {
            let fees = fees.context("coordinator fee configuration was not received")?;
            let fee_units = fees
                .job_price_units
                .checked_sub(fees.provider_payout_units)
                .context("provider payout exceeds the job price")?;
            (
                EarningsStatus::Completed,
                Some(fees.job_price_units),
                Some(fees.provider_payout_units),
                Some(fee_units),
            )
        } else {
            (EarningsStatus::Failed, None, None, None)
        };
        entries.push(EarningsEntry {
            job_id: job_id.to_string(),
            timestamp_unix_ms: timestamp_unix_ms(),
            status,
            gross_units,
            provider_payout_units,
            fee_units,
        });
        let entries = self.persist(entries)?;
        Ok(snapshot(entries, recovered_corruption))
    }

    fn read_entries(&self) -> Result<(Vec<EarningsEntry>, bool)> {
        if !self.path.exists() {
            return Ok((Vec::new(), false));
        }
        let contents = fs::read(&self.path)
            .with_context(|| format!("failed to read earnings history: {}", self.path.display()))?;
        match serde_json::from_slice::<Vec<EarningsEntry>>(&contents) {
            Ok(entries) => Ok((entries, false)),
            Err(error) => {
                let corrupt_path = self.corrupt_path();
                fs::rename(&self.path, &corrupt_path).with_context(|| {
                    format!(
                        "failed to preserve corrupt earnings history at {}: {error}",
                        corrupt_path.display()
                    )
                })?;
                self.persist(Vec::new())?;
                Ok((Vec::new(), true))
            }
        }
    }

    fn persist(&self, mut entries: Vec<EarningsEntry>) -> Result<Vec<EarningsEntry>> {
        if entries.len() > MAX_HISTORY_ENTRIES {
            let excess = entries.len() - MAX_HISTORY_ENTRIES;
            entries.drain(..excess);
        }
        if encoded_size(&entries)? > MAX_HISTORY_BYTES {
            let mut first_retained = 0;
            let mut first_fitting = entries.len();
            while first_retained < first_fitting {
                let middle = first_retained + (first_fitting - first_retained) / 2;
                if encoded_size(&entries[middle..])? <= MAX_HISTORY_BYTES {
                    first_fitting = middle;
                } else {
                    first_retained = middle + 1;
                }
            }
            if first_retained == entries.len() {
                bail!("a history entry exceeds the earnings-history size limit");
            }
            entries.drain(..first_retained);
        }
        let bytes =
            serde_json::to_vec_pretty(&entries).context("failed to encode earnings history")?;
        let parent = self
            .path
            .parent()
            .context("earnings history path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create history directory: {}", parent.display()))?;
        let temp_path = self.path.with_extension("json.tmp");
        fs::write(&temp_path, bytes).with_context(|| {
            format!("failed to write earnings history: {}", temp_path.display())
        })?;
        if self.path.exists() {
            fs::remove_file(&self.path).with_context(|| {
                format!(
                    "failed to replace earnings history: {}",
                    self.path.display()
                )
            })?;
        }
        fs::rename(&temp_path, &self.path).with_context(|| {
            format!(
                "failed to install earnings history: {}",
                self.path.display()
            )
        })?;
        Ok(entries)
    }

    fn needs_rotation(&self, entries: &[EarningsEntry]) -> Result<bool> {
        Ok(entries.len() > MAX_HISTORY_ENTRIES || encoded_size(entries)? > MAX_HISTORY_BYTES)
    }

    fn corrupt_path(&self) -> PathBuf {
        let timestamp = timestamp_unix_ms();
        let name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("earnings.json");
        self.path
            .with_file_name(format!("{name}.corrupt-{timestamp}"))
    }
}

fn encoded_size(entries: &[EarningsEntry]) -> Result<usize> {
    serde_json::to_vec_pretty(entries)
        .map(|bytes| bytes.len())
        .context("failed to size earnings history")
}

fn snapshot(entries: Vec<EarningsEntry>, recovered_corruption: bool) -> EarningsSnapshot {
    let balance_units = entries
        .iter()
        .filter(|entry| entry.status == EarningsStatus::Completed)
        .filter_map(|entry| entry.provider_payout_units)
        .fold(0_u64, u64::saturating_add);
    EarningsSnapshot {
        job_count: entries.len(),
        entries,
        balance_units,
        recovered_corruption,
    }
}

fn timestamp_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{EarningsEntry, EarningsStatus, EarningsStore, MAX_HISTORY_BYTES};
    use provider_core::FeeConfig;

    fn test_path() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("gpuswarm-history-{}-{suffix}", std::process::id()))
            .join("earnings.json")
    }

    fn remove_test_dir(path: &Path) {
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn recovers_corrupt_history_without_losing_the_original_file() {
        let path = test_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{not-json").unwrap();
        let store = EarningsStore::new(path.clone());

        let earnings = store.load().unwrap();

        assert!(earnings.recovered_corruption);
        assert!(earnings.entries.is_empty());
        assert_eq!(fs::read(&path).unwrap(), b"[]");
        assert!(fs::read_dir(path.parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".corrupt-")));
        remove_test_dir(&path);
    }

    #[test]
    fn rotates_history_to_the_size_limit_and_preserves_newest_entries() {
        let path = test_path();
        let store = EarningsStore::new(path.clone());
        let entries = (0..3000)
            .map(|index| EarningsEntry {
                job_id: format!("{index:04}-{}", "x".repeat(480)),
                timestamp_unix_ms: index,
                status: EarningsStatus::Completed,
                gross_units: Some(10_000),
                provider_payout_units: Some(9_000),
                fee_units: Some(1_000),
            })
            .collect();

        store.persist(entries).unwrap();

        assert!(fs::metadata(&path).unwrap().len() as usize <= MAX_HISTORY_BYTES);
        let rotated = store.load().unwrap();
        assert!(rotated.entries.len() < 3000);
        assert!(rotated.entries.last().unwrap().job_id.starts_with("2999-"));
        assert!(!rotated.entries[0].job_id.starts_with("0000-"));
        remove_test_dir(&path);
    }

    #[test]
    fn records_integer_unit_settlement_from_coordinator_fee_config() {
        let path = test_path();
        let store = EarningsStore::new(path.clone());
        let fees = FeeConfig {
            fee_bps: 1_000,
            job_price_units: 10_000,
            provider_payout_units: 9_000,
            fee_address: "0x1111111111111111111111111111111111111111".into(),
        };

        let earnings = store.record_result("job-1", true, Some(&fees)).unwrap();

        assert_eq!(earnings.balance_units, 9_000);
        assert_eq!(earnings.job_count, 1);
        assert_eq!(earnings.entries[0].gross_units, Some(10_000));
        assert_eq!(earnings.entries[0].provider_payout_units, Some(9_000));
        assert_eq!(earnings.entries[0].fee_units, Some(1_000));
        remove_test_dir(&path);
    }
}
