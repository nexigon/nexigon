//! Polling and execution of device operation command steps.
//!
//! When enabled, [`run`] claims the command steps of device operations from
//! Nexigon Hub, executes them with the registered commands, and reports their
//! results. The [`OperationLedger`] records every dispatch durably, so that a
//! completed result is reported again without executing the command again, and
//! an execution interrupted after dispatch is failed instead of replayed. The
//! poller runs for the lifetime of the agent: a lost connection pauses polling
//! but does not interrupt a running command.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use nexigon_api::types::devices::ClaimDeviceOperationWorkAction;
use nexigon_api::types::devices::DeviceCommandInvokeData;
use nexigon_api::types::devices::DeviceCommandStatus;
use nexigon_api::types::devices::DeviceOperationId;
use nexigon_api::types::devices::DeviceOperationStepReport;
use nexigon_api::types::devices::DeviceOperationStepReportStatus;
use nexigon_api::types::devices::DeviceOperationWorkKind;
use nexigon_api::types::devices::DeviceOperationWorkStep;
use nexigon_api::types::devices::ReportDeviceOperationStepAction;
use nexigon_client::Execute;
use nexigon_ids::ids::DeviceId;
use nexigon_ids::ids::DeviceOperationWorkClaimId;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::config::Config;
use crate::config::OperationsConfig;
use crate::device_api::DeviceExecutor;
use crate::handlers;
use crate::handlers::CommandRegistry;
use crate::hub_link::HubLink;
use crate::operation_ledger::OperationLedger;
use crate::operation_ledger::PreviousExecution;

/// Default time between polls that found no work.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Whether operation polling is enabled, which requires an explicit opt-in.
pub(crate) fn polling_enabled(config: Option<&OperationsConfig>) -> bool {
    config
        .and_then(|operations| operations.enabled)
        .unwrap_or(false)
}

/// Time between polls that found no work.
pub(crate) fn poll_interval(config: &Config) -> Duration {
    config
        .operations
        .as_ref()
        .and_then(|operations| operations.poll_interval_secs)
        .map_or(DEFAULT_POLL_INTERVAL, Duration::from_secs)
}

/// Dependencies of the operation poller.
pub(crate) struct OperationPoller {
    pub(crate) link: HubLink,
    pub(crate) device_id: DeviceId,
    pub(crate) command_registry: Option<Arc<CommandRegistry>>,
    pub(crate) command_slots: Arc<Semaphore>,
    pub(crate) ledger: OperationLedger,
    pub(crate) poll_interval: Duration,
}

/// Claim, execute, and report command steps until cancelled.
pub(crate) async fn run(
    poller: OperationPoller,
    cancellation: CancellationToken,
) -> anyhow::Result<()> {
    let OperationPoller {
        link,
        device_id,
        command_registry,
        command_slots,
        mut ledger,
        poll_interval,
    } = poller;
    let mut executor = link.executor();
    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(()),
            () = link.connected() => {}
        }
        // Reserve execution capacity before claiming work so a lease never
        // waits behind already-running interactive commands.
        let command_permit = if command_registry.is_some() {
            let permit = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok(()),
                permit = command_slots.clone().acquire_owned() => permit,
            };
            match permit {
                Ok(permit) => Some(permit),
                Err(_) => {
                    anyhow::bail!("command concurrency limiter was closed");
                }
            }
        } else {
            None
        };
        // Whether this poll found anything. The hub releases a device operation's
        // next step as soon as the previous one is reported, so a poll that found
        // work is very likely to find more; sleeping out the whole idle interval
        // between the steps of one operation would waste that. Only an empty poll
        // means there is genuinely nothing to do.
        let mut found_work = false;
        let claim = executor.execute(
            ClaimDeviceOperationWorkAction::new(device_id.clone())
                // Commands are executed serially. Lease only the command we
                // are about to execute so later work cannot expire while an
                // earlier handler is still running.
                .with_limit(Some(1))
                .with_kinds(Some(vec![DeviceOperationWorkKind::DeviceCommand].into())),
        );
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(()),
            result = claim => result,
        };
        match result {
            Ok(Ok(output)) => {
                for item in output.work {
                    let device_operation_id = item.device_operation_id;
                    let step_index = item.step_index;
                    let claim_id = item.claim_id;
                    let report = match ledger.previous(&device_operation_id, step_index) {
                        PreviousExecution::Completed(report) => {
                            // A completed result may be returning under a fresh
                            // lease after an earlier reporting outage. Commit the
                            // current claim before trying it again so a crash
                            // cannot leave the outbox tied to the expired claim.
                            if let Err(error) = ledger
                                .mark_completed(
                                    &device_operation_id,
                                    step_index,
                                    &claim_id,
                                    report.clone(),
                                )
                                .await
                            {
                                warn!(?error, "failed to persist renewed operation claim");
                                continue;
                            }
                            report
                        }
                        PreviousExecution::InProgress => {
                            let report = DeviceOperationStepReport {
                                status: DeviceOperationStepReportStatus::Failed,
                                output: None,
                                error: Some(
                                    ("previous command execution was interrupted after dispatch; refusing unsafe automatic replay"
                                        .to_owned()).into(),
                                ),
                            };
                            if let Err(error) = ledger
                                .mark_completed(
                                    &device_operation_id,
                                    step_index,
                                    &claim_id,
                                    report.clone(),
                                )
                                .await
                            {
                                warn!(?error, "failed to persist interrupted operation result");
                                continue;
                            }
                            report
                        }
                        PreviousExecution::None => match item.step {
                            DeviceOperationWorkStep::DeviceCommand(step) => {
                                if let Err(error) = ledger
                                    .mark_in_progress(&device_operation_id, step_index, &claim_id)
                                    .await
                                {
                                    warn!(
                                        ?error,
                                        "failed to persist operation dispatch; command not executed"
                                    );
                                    continue;
                                }
                                let done = if let Some(registry) = command_registry.as_ref() {
                                    let request =
                                        DeviceCommandInvokeData::new(step.command, step.input)
                                            .with_stream_log(Some(false))
                                            .with_timeout_secs(Some(
                                                step.timeout_secs.unwrap_or(3600),
                                            ));
                                    handlers::invoke_registered_command_with_cancellation(
                                        registry,
                                        request,
                                        &cancellation,
                                    )
                                    .await
                                } else {
                                    nexigon_api::types::devices::DeviceCommandDoneData {
                                        status: DeviceCommandStatus::Error,
                                        output: None,
                                        error: Some("commands not enabled".into()),
                                        log_tail: Vec::new().into(),
                                        duration_ms: 0,
                                    }
                                };
                                if cancellation.is_cancelled() {
                                    return Ok(());
                                }
                                let status = match done.status {
                                    DeviceCommandStatus::Ok => {
                                        DeviceOperationStepReportStatus::Succeeded
                                    }
                                    DeviceCommandStatus::Error => {
                                        DeviceOperationStepReportStatus::Failed
                                    }
                                };
                                let report = DeviceOperationStepReport {
                                    status,
                                    output: done.output,
                                    error: done.error,
                                };
                                if let Err(error) = ledger
                                    .mark_completed(
                                        &device_operation_id,
                                        step_index,
                                        &claim_id,
                                        report.clone(),
                                    )
                                    .await
                                {
                                    warn!(
                                        ?error,
                                        "failed to persist operation result; result not reported"
                                    );
                                    continue;
                                }
                                report
                            }
                        },
                    };
                    found_work = true;
                    // Retry transport failures promptly while this claim's lease
                    // is certainly still current. The shortest valid command
                    // lease is 61 seconds; these five attempts span 15 seconds.
                    // A persistent outage falls back to normal lease expiry and
                    // reacquisition, at which point the saved result is submitted
                    // under the fresh claim without executing again.
                    let acknowledged = report_operation_step_with_retry(
                        &mut executor,
                        &device_id,
                        &device_operation_id,
                        step_index,
                        &claim_id,
                        &report,
                        Some(&cancellation),
                    )
                    .await;
                    if acknowledged
                        && let Err(error) = ledger.remove(&device_operation_id, step_index).await
                    {
                        warn!(?error, "failed to prune reported operation result");
                    }
                }
            }
            Ok(Err(error)) => {
                warn!(message = %error.message, "operation work claim rejected");
            }
            Err(error) => {
                warn!(?error, "failed to claim operation work");
            }
        }
        drop(command_permit);
        if !found_work {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok(()),
                () = tokio::time::sleep(poll_interval) => {}
            }
        }
    }
}

#[derive(Debug)]
enum ReportAttempt {
    Acknowledged,
    Rejected(String),
    TransportFailed(String),
}

trait OperationReporter {
    fn report(
        &mut self,
        device_id: DeviceId,
        operation_id: DeviceOperationId,
        step_index: u32,
        claim_id: DeviceOperationWorkClaimId,
        report: DeviceOperationStepReport,
    ) -> impl Future<Output = ReportAttempt>;
}

impl<E: Execute> OperationReporter for DeviceExecutor<E> {
    async fn report(
        &mut self,
        device_id: DeviceId,
        operation_id: DeviceOperationId,
        step_index: u32,
        claim_id: DeviceOperationWorkClaimId,
        report: DeviceOperationStepReport,
    ) -> ReportAttempt {
        match self
            .execute(ReportDeviceOperationStepAction::new(
                device_id,
                operation_id,
                step_index,
                claim_id,
                report,
            ))
            .await
        {
            Ok(Ok(_)) => ReportAttempt::Acknowledged,
            Ok(Err(error)) => ReportAttempt::Rejected(error.message.into()),
            Err(error) => ReportAttempt::TransportFailed(error.to_string()),
        }
    }
}

async fn report_operation_step_with_retry(
    reporter: &mut impl OperationReporter,
    device_id: &DeviceId,
    operation_id: &DeviceOperationId,
    step_index: u32,
    claim_id: &DeviceOperationWorkClaimId,
    report: &DeviceOperationStepReport,
    cancellation: Option<&CancellationToken>,
) -> bool {
    let mut retry_delay = Duration::from_secs(1);
    for attempt in 1..=5 {
        let report_attempt = reporter.report(
            device_id.clone(),
            operation_id.clone(),
            step_index,
            claim_id.clone(),
            report.clone(),
        );
        tokio::pin!(report_attempt);
        let report_attempt = if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return false,
                result = &mut report_attempt => result,
            }
        } else {
            report_attempt.await
        };
        match report_attempt {
            ReportAttempt::Acknowledged => return true,
            ReportAttempt::Rejected(message) => {
                warn!(%message, "operation step report rejected");
                return false;
            }
            ReportAttempt::TransportFailed(error) => {
                warn!(%error, attempt, "failed to report operation step");
                if attempt < 5 {
                    if let Some(cancellation) = cancellation {
                        tokio::select! {
                            biased;
                            () = cancellation.cancelled() => return false,
                            () = tokio::time::sleep(retry_delay) => {}
                        }
                    } else {
                        tokio::time::sleep(retry_delay).await;
                    }
                    retry_delay *= 2;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use nexigon_api::types::devices::DeviceOperationId;
    use nexigon_api::types::devices::DeviceOperationStepReport;
    use nexigon_api::types::devices::DeviceOperationStepReportStatus;
    use nexigon_ids::Generate;
    use nexigon_ids::ids::DeviceId;
    use nexigon_ids::ids::DeviceOperationWorkClaimId;

    use super::OperationReporter;
    use super::ReportAttempt;
    use super::polling_enabled;
    use super::report_operation_step_with_retry;
    use crate::config::OperationsConfig;

    #[test]
    fn operation_polling_requires_explicit_opt_in() {
        assert!(!polling_enabled(None));
        assert!(!polling_enabled(Some(&OperationsConfig::new())));
        assert!(!polling_enabled(Some(
            &OperationsConfig::new().with_enabled(Some(false)),
        )));
        assert!(polling_enabled(Some(
            &OperationsConfig::new().with_enabled(Some(true)),
        )));
    }

    struct MockReporter {
        attempts: VecDeque<ReportAttempt>,
        calls: usize,
    }

    impl OperationReporter for MockReporter {
        async fn report(
            &mut self,
            _: DeviceId,
            _: DeviceOperationId,
            _: u32,
            _: DeviceOperationWorkClaimId,
            _: DeviceOperationStepReport,
        ) -> ReportAttempt {
            self.calls += 1;
            self.attempts.pop_front().expect("one result per call")
        }
    }

    #[tokio::test(start_paused = true)]
    async fn completed_operation_reports_retry_transport_failures() {
        let mut reporter = MockReporter {
            attempts: VecDeque::from([
                ReportAttempt::TransportFailed("offline".to_owned()),
                ReportAttempt::TransportFailed("still offline".to_owned()),
                ReportAttempt::Acknowledged,
            ]),
            calls: 0,
        };
        let acknowledged = report_operation_step_with_retry(
            &mut reporter,
            &DeviceId::generate(),
            &DeviceOperationId::generate(),
            0,
            &DeviceOperationWorkClaimId::generate(),
            &DeviceOperationStepReport::new(DeviceOperationStepReportStatus::Succeeded),
            None,
        )
        .await;

        assert!(acknowledged);
        assert_eq!(reporter.calls, 3);
    }
}
