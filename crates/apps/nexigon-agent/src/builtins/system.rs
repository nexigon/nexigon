//! System power commands: reboot and shutdown.

use std::pin::Pin;

use nexigon_api::types::devices::DeviceCommandDoneData;
use nexigon_api::types::devices::DeviceCommandStatus;
use nexigon_api::types::properties::DeviceCommandDescriptor;

use super::BuiltinCommand;
use super::InvocationCtx;

pub fn commands() -> Vec<Box<dyn BuiltinCommand>> {
    vec![Box::new(RebootCommand), Box::new(ShutdownCommand)]
}

struct RebootCommand;

impl BuiltinCommand for RebootCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.system.reboot".to_owned(),
            description: Some("Reboot the system".to_owned()),
            category: Some("power".to_owned()),
            input: None,
            output: None,
        }
    }

    fn execute(
        &self,
        _ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(run_command("reboot", &[]))
    }
}

struct ShutdownCommand;

impl BuiltinCommand for ShutdownCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.system.shutdown".to_owned(),
            description: Some("Shut down the system".to_owned()),
            category: Some("power".to_owned()),
            input: None,
            output: None,
        }
    }

    fn execute(
        &self,
        _ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(run_command("shutdown", &["-h", "now"]))
    }
}

async fn run_command(program: &str, args: &[&str]) -> DeviceCommandDoneData {
    let started = std::time::Instant::now();
    match tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let log_tail: Vec<String> = stderr.lines().map(|l| l.to_owned()).collect();
            if output.status.success() {
                DeviceCommandDoneData {
                    status: DeviceCommandStatus::Ok,
                    output: None,
                    error: None,
                    log_tail,
                    duration_ms: started.elapsed().as_millis() as u64,
                }
            } else {
                DeviceCommandDoneData {
                    status: DeviceCommandStatus::Error,
                    output: None,
                    error: Some(format!("{program} exited with status {}", output.status)),
                    log_tail,
                    duration_ms: started.elapsed().as_millis() as u64,
                }
            }
        }
        Err(e) => DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("failed to run {program}: {e}")),
            log_tail: Vec::new(),
            duration_ms: started.elapsed().as_millis() as u64,
        },
    }
}
