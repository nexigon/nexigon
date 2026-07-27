//! Durable deduplication for device-operation command execution.
//!
//! Every execution is one atomic file. A command can finish locally while its report is
//! lost, and the hub cannot make that external side effect exactly-once, so the agent
//! persists dispatch before execution and the completed report before sending it. A
//! completed result is sent again when the work is leased again without re-executing;
//! an execution interrupted after dispatch is failed conservatively rather than replayed.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use crate::config::operation_ledger::OperationExecutionCompleted;
use crate::config::operation_ledger::OperationExecutionEntry;
use crate::config::operation_ledger::OperationExecutionInProgress;
use anyhow::Context;
use anyhow::bail;
use nexigon_api::types::devices::DeviceOperationId;
use nexigon_api::types::devices::DeviceOperationStepReport;
use nexigon_ids::Id;
use nexigon_ids::ids::DeviceOperationWorkClaimId;

const LEDGER_DIRECTORY_NAME: &str = "operation-executions";
const LARGE_LEDGER_ENTRY_COUNT: usize = 1_000;

pub(super) enum PreviousExecution {
    None,
    InProgress,
    Completed(DeviceOperationStepReport),
}

pub(super) struct OperationLedger {
    directory: PathBuf,
    entries: BTreeMap<String, OperationExecutionEntry>,
}

impl OperationLedger {
    pub(super) async fn load(data_path: &Path) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(data_path)
            .await
            .with_context(|| format!("creating agent data directory {}", data_path.display()))?;
        let directory = data_path.join(LEDGER_DIRECTORY_NAME);
        tokio::fs::create_dir_all(&directory)
            .await
            .with_context(|| format!("creating operation ledger {}", directory.display()))?;

        let mut ledger = Self {
            directory,
            entries: BTreeMap::new(),
        };
        ledger.load_entries().await?;
        Ok(ledger)
    }

    pub(super) fn previous(
        &self,
        operation_id: &DeviceOperationId,
        step_index: u32,
    ) -> PreviousExecution {
        match self.entries.get(&key(operation_id, step_index)) {
            None => PreviousExecution::None,
            Some(OperationExecutionEntry::InProgress(_)) => PreviousExecution::InProgress,
            Some(OperationExecutionEntry::Completed(entry)) => {
                PreviousExecution::Completed(entry.report.clone())
            }
        }
    }

    pub(super) async fn mark_in_progress(
        &mut self,
        operation_id: &DeviceOperationId,
        step_index: u32,
        claim_id: &DeviceOperationWorkClaimId,
    ) -> anyhow::Result<()> {
        self.replace(
            operation_id,
            step_index,
            OperationExecutionEntry::InProgress(OperationExecutionInProgress {
                device_operation_id: operation_id.clone(),
                step_index,
                claim_id: claim_id.clone(),
            }),
        )
        .await
    }

    pub(super) async fn mark_completed(
        &mut self,
        operation_id: &DeviceOperationId,
        step_index: u32,
        claim_id: &DeviceOperationWorkClaimId,
        report: DeviceOperationStepReport,
    ) -> anyhow::Result<()> {
        self.replace(
            operation_id,
            step_index,
            OperationExecutionEntry::Completed(OperationExecutionCompleted {
                device_operation_id: operation_id.clone(),
                step_index,
                claim_id: claim_id.clone(),
                report,
            }),
        )
        .await
    }

    pub(super) async fn remove(
        &mut self,
        operation_id: &DeviceOperationId,
        step_index: u32,
    ) -> anyhow::Result<()> {
        let path = self.entry_path(operation_id, step_index);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("removing operation ledger entry {}", path.display())
                });
            }
        }
        sync_directory(&self.directory).await?;
        self.entries.remove(&key(operation_id, step_index));
        Ok(())
    }

    async fn replace(
        &mut self,
        operation_id: &DeviceOperationId,
        step_index: u32,
        entry: OperationExecutionEntry,
    ) -> anyhow::Result<()> {
        // Persist the prospective value first. If any filesystem operation fails, the
        // in-memory view continues to describe the last successfully committed file.
        self.persist_entry(operation_id, step_index, &entry).await?;
        self.entries.insert(key(operation_id, step_index), entry);
        Ok(())
    }

    async fn persist_entry(
        &self,
        operation_id: &DeviceOperationId,
        step_index: u32,
        entry: &OperationExecutionEntry,
    ) -> anyhow::Result<()> {
        let bytes =
            serde_json::to_vec_pretty(entry).context("serializing operation execution entry")?;
        let path = self.entry_path(operation_id, step_index);
        let temporary = path.with_extension("json.tmp");
        tokio::fs::write(&temporary, bytes)
            .await
            .with_context(|| format!("writing operation ledger entry {}", temporary.display()))?;
        tokio::fs::File::open(&temporary)
            .await
            .with_context(|| format!("opening operation ledger entry {}", temporary.display()))?
            .sync_all()
            .await
            .with_context(|| format!("syncing operation ledger entry {}", temporary.display()))?;
        tokio::fs::rename(&temporary, &path)
            .await
            .with_context(|| format!("committing operation ledger entry {}", path.display()))?;
        sync_directory(&self.directory).await
    }

    async fn load_entries(&mut self) -> anyhow::Result<()> {
        let mut directory = tokio::fs::read_dir(&self.directory)
            .await
            .with_context(|| format!("reading operation ledger {}", self.directory.display()))?;
        while let Some(item) = directory.next_entry().await? {
            let path = item.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("tmp") {
                // A crash before rename can leave only a temporary file. It was never a
                // committed dispatch record, so it is safe to discard.
                let _ = tokio::fs::remove_file(path).await;
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let bytes = tokio::fs::read(&path)
                .await
                .with_context(|| format!("reading operation ledger entry {}", path.display()))?;
            let entry: OperationExecutionEntry = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing operation ledger entry {}", path.display()))?;
            let (operation_id, step_index) = entry_identity(&entry);
            if self.entry_path(operation_id, step_index) != path {
                bail!(
                    "operation ledger entry has inconsistent filename: {}",
                    path.display()
                );
            }
            self.entries.insert(key(operation_id, step_index), entry);
        }
        if self.entries.len() >= LARGE_LEDGER_ENTRY_COUNT {
            tracing::warn!(
                entries = self.entries.len(),
                path = %self.directory.display(),
                "operation execution ledger contains unusually many pending entries"
            );
        }
        Ok(())
    }

    fn entry_path(&self, operation_id: &DeviceOperationId, step_index: u32) -> PathBuf {
        self.directory
            .join(format!("{}.{}.json", operation_id.stringify(), step_index))
    }
}

fn entry_identity(entry: &OperationExecutionEntry) -> (&DeviceOperationId, u32) {
    match entry {
        OperationExecutionEntry::InProgress(entry) => {
            (&entry.device_operation_id, entry.step_index)
        }
        OperationExecutionEntry::Completed(entry) => (&entry.device_operation_id, entry.step_index),
    }
}

async fn sync_directory(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        tokio::fs::File::open(path)
            .await
            .with_context(|| format!("opening directory {}", path.display()))?
            .sync_all()
            .await
            .with_context(|| format!("syncing directory {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn key(operation_id: &DeviceOperationId, step_index: u32) -> String {
    format!("{}:{step_index}", operation_id.stringify())
}

#[cfg(test)]
mod tests {
    use nexigon_api::types::devices::DeviceOperationStepReportStatus;
    use nexigon_ids::Generate;

    use super::*;

    #[tokio::test]
    async fn persists_each_execution_as_an_independent_file() {
        let first_operation_id = DeviceOperationId::generate();
        let second_operation_id = DeviceOperationId::generate();
        let first_claim_id = DeviceOperationWorkClaimId::generate();
        let second_claim_id = DeviceOperationWorkClaimId::generate();
        let directory = temporary_directory(&first_operation_id);

        let mut ledger = OperationLedger::load(&directory).await.unwrap();
        ledger
            .mark_in_progress(&first_operation_id, 3, &first_claim_id)
            .await
            .unwrap();
        ledger
            .mark_in_progress(&second_operation_id, 1, &second_claim_id)
            .await
            .unwrap();
        assert!(ledger.entry_path(&first_operation_id, 3).is_file());
        assert!(ledger.entry_path(&second_operation_id, 1).is_file());
        let stored: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(ledger.entry_path(&first_operation_id, 3))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(stored.get("version").is_none());
        assert_eq!(stored["state"], "inProgress");
        assert_eq!(stored["claimId"], first_claim_id.stringify());

        let report = DeviceOperationStepReport::new(DeviceOperationStepReportStatus::Succeeded)
            .with_output(Some(serde_json::json!({"ok": true})));
        ledger
            .mark_completed(&first_operation_id, 3, &first_claim_id, report)
            .await
            .unwrap();
        ledger.remove(&second_operation_id, 1).await.unwrap();

        let ledger = OperationLedger::load(&directory).await.unwrap();
        let PreviousExecution::Completed(report) = ledger.previous(&first_operation_id, 3) else {
            panic!("completed result was not recovered");
        };
        assert_eq!(report.output, Some(serde_json::json!({"ok": true})));
        assert!(matches!(
            ledger.previous(&second_operation_id, 1),
            PreviousExecution::None
        ));

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn failed_persistence_does_not_change_the_in_memory_state() {
        let operation_id = DeviceOperationId::generate();
        let claim_id = DeviceOperationWorkClaimId::generate();
        let directory = temporary_directory(&operation_id);
        let mut ledger = OperationLedger::load(&directory).await.unwrap();

        // A directory at the destination makes the atomic rename fail after the
        // temporary file has been written and synced.
        tokio::fs::create_dir(ledger.entry_path(&operation_id, 0))
            .await
            .unwrap();
        assert!(
            ledger
                .mark_in_progress(&operation_id, 0, &claim_id)
                .await
                .is_err()
        );
        assert!(matches!(
            ledger.previous(&operation_id, 0),
            PreviousExecution::None
        ));

        let _ = tokio::fs::remove_dir_all(directory).await;
    }

    fn temporary_directory(operation_id: &DeviceOperationId) -> PathBuf {
        std::env::temp_dir().join(format!("nexigon-operation-ledger-{operation_id}"))
    }
}
