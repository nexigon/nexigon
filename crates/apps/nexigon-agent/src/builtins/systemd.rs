//! Systemd service management commands.

use std::pin::Pin;

use nexigon_api::types::devices::DeviceCommandDoneData;
use nexigon_api::types::devices::DeviceCommandStatus;
use nexigon_api::types::properties::DeviceCommandDescriptor;

use super::BuiltinCommand;
use super::InvocationCtx;

pub fn commands() -> Vec<Box<dyn BuiltinCommand>> {
    vec![
        Box::new(ListUnitsCommand),
        Box::new(RestartCommand),
        Box::new(StatusCommand),
    ]
}

struct ListUnitsCommand;

impl BuiltinCommand for ListUnitsCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.systemd.list-units".to_owned(),
            description: Some("List systemd units".to_owned()),
            category: Some("services".to_owned()),
            input: None,
            output: None,
        }
    }

    fn execute(
        &self,
        _ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(async { run_systemctl_json(&["list-units", "--output=json"]).await })
    }
}

struct RestartCommand;

impl BuiltinCommand for RestartCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.systemd.restart".to_owned(),
            description: Some("Restart a systemd unit".to_owned()),
            category: Some("services".to_owned()),
            input: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "unit": {
                        "type": "string",
                        "description": "Name of the systemd unit to restart"
                    }
                },
                "required": ["unit"]
            })),
            output: None,
        }
    }

    fn execute(
        &self,
        ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(async move {
            let unit = match extract_unit(&ctx.input) {
                Ok(unit) => unit,
                Err(done) => return done,
            };
            run_systemctl_status(&["restart", &unit]).await
        })
    }
}

struct StatusCommand;

impl BuiltinCommand for StatusCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.systemd.status".to_owned(),
            description: Some("Get the status of a systemd unit".to_owned()),
            category: Some("services".to_owned()),
            input: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "unit": {
                        "type": "string",
                        "description": "Name of the systemd unit"
                    }
                },
                "required": ["unit"]
            })),
            output: None,
        }
    }

    fn execute(
        &self,
        ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(async move {
            let unit = match extract_unit(&ctx.input) {
                Ok(unit) => unit,
                Err(done) => return done,
            };
            run_systemctl_show(&unit).await
        })
    }
}

/// Extract the `unit` field from the input JSON.
fn extract_unit(input: &serde_json::Value) -> Result<String, DeviceCommandDoneData> {
    #[derive(serde::Deserialize)]
    struct UnitInput {
        unit: String,
    }

    match serde_json::from_value::<UnitInput>(input.clone()) {
        Ok(parsed) => Ok(parsed.unit),
        Err(e) => Err(DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("invalid input: {e}")),
            log_tail: Vec::new(),
            duration_ms: 0,
        }),
    }
}

/// Run `systemctl show` and parse its key=value output into a JSON object.
async fn run_systemctl_show(unit: &str) -> DeviceCommandDoneData {
    let started = std::time::Instant::now();
    match tokio::process::Command::new("systemctl")
        .args(["show", unit])
        .stdin(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let log_tail: Vec<String> = stderr.lines().map(|l| l.to_owned()).collect();
            let duration_ms = started.elapsed().as_millis() as u64;

            if !output.status.success() {
                return DeviceCommandDoneData {
                    status: DeviceCommandStatus::Error,
                    output: None,
                    error: Some(format!("systemctl exited with status {}", output.status)),
                    log_tail,
                    duration_ms,
                };
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut map = serde_json::Map::new();
            for line in stdout.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    map.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
                }
            }

            DeviceCommandDoneData {
                status: DeviceCommandStatus::Ok,
                output: Some(serde_json::Value::Object(map)),
                error: None,
                log_tail,
                duration_ms,
            }
        }
        Err(e) => DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("failed to run systemctl: {e}")),
            log_tail: Vec::new(),
            duration_ms: started.elapsed().as_millis() as u64,
        },
    }
}

/// Run a systemctl command and return its stdout parsed as JSON.
async fn run_systemctl_json(args: &[&str]) -> DeviceCommandDoneData {
    let started = std::time::Instant::now();
    match tokio::process::Command::new("systemctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let log_tail: Vec<String> = stderr.lines().map(|l| l.to_owned()).collect();
            let duration_ms = started.elapsed().as_millis() as u64;

            if !output.status.success() {
                return DeviceCommandDoneData {
                    status: DeviceCommandStatus::Error,
                    output: None,
                    error: Some(format!("systemctl exited with status {}", output.status)),
                    log_tail,
                    duration_ms,
                };
            }

            match serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                Ok(json) => DeviceCommandDoneData {
                    status: DeviceCommandStatus::Ok,
                    output: Some(json),
                    error: None,
                    log_tail,
                    duration_ms,
                },
                Err(e) => DeviceCommandDoneData {
                    status: DeviceCommandStatus::Error,
                    output: None,
                    error: Some(format!("failed to parse systemctl JSON output: {e}")),
                    log_tail,
                    duration_ms,
                },
            }
        }
        Err(e) => DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("failed to run systemctl: {e}")),
            log_tail: Vec::new(),
            duration_ms: started.elapsed().as_millis() as u64,
        },
    }
}

/// Run a systemctl command and return just the exit status (no JSON output).
async fn run_systemctl_status(args: &[&str]) -> DeviceCommandDoneData {
    let started = std::time::Instant::now();
    match tokio::process::Command::new("systemctl")
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
                    error: Some(format!("systemctl exited with status {}", output.status)),
                    log_tail,
                    duration_ms: started.elapsed().as_millis() as u64,
                }
            }
        }
        Err(e) => DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("failed to run systemctl: {e}")),
            log_tail: Vec::new(),
            duration_ms: started.elapsed().as_millis() as u64,
        },
    }
}
