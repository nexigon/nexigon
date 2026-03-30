//! Rugix Apps lifecycle management commands.

use std::pin::Pin;

use nexigon_api::types::devices::DeviceCommandDoneData;
use nexigon_api::types::devices::DeviceCommandStatus;
use nexigon_api::types::properties::DeviceCommandDescriptor;
use nexigon_api::types::repositories::GetPackageVersionDetailsAction;
use nexigon_api::types::repositories::IssueAssetDownloadUrlAction;
use nexigon_client::connect_executor;
use nexigon_ids::ids::PackageVersionId;

use super::BuiltinCommand;
use super::InvocationCtx;

pub fn commands() -> Vec<Box<dyn BuiltinCommand>> {
    vec![
        Box::new(ListCommand),
        Box::new(InfoCommand),
        Box::new(DeployCommand),
        Box::new(StartCommand),
        Box::new(StopCommand),
        Box::new(RollbackCommand),
        Box::new(RemoveCommand),
    ]
}

#[derive(serde::Deserialize)]
struct AppInput {
    app: String,
}

#[derive(serde::Deserialize)]
struct DeployInput {
    /// Package version ID to deploy.
    #[serde(rename = "versionId")]
    version_id: PackageVersionId,
    /// Optional asset filename (defaults to first `.rugixb` asset).
    #[serde(rename = "assetFilename")]
    asset_filename: Option<String>,
}

fn extract_app(input: &serde_json::Value) -> Result<String, DeviceCommandDoneData> {
    match serde_json::from_value::<AppInput>(input.clone()) {
        Ok(parsed) => Ok(parsed.app),
        Err(e) => Err(DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("invalid input: {e}")),
            log_tail: Vec::new(),
            duration_ms: 0,
        }),
    }
}

fn extract_deploy_input(input: &serde_json::Value) -> Result<DeployInput, DeviceCommandDoneData> {
    match serde_json::from_value::<DeployInput>(input.clone()) {
        Ok(parsed) => Ok(parsed),
        Err(e) => Err(DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("invalid input: {e}")),
            log_tail: Vec::new(),
            duration_ms: 0,
        }),
    }
}

/// Run `rugix-ctrl` with the given arguments and return its stdout parsed as JSON.
async fn run_rugix_ctrl_json(args: &[&str]) -> DeviceCommandDoneData {
    let started = std::time::Instant::now();
    match tokio::process::Command::new("rugix-ctrl")
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
                    error: Some(format!("rugix-ctrl exited with status {}", output.status)),
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
                    error: Some(format!("failed to parse rugix-ctrl output: {e}")),
                    log_tail,
                    duration_ms,
                },
            }
        }
        Err(e) => DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("failed to run rugix-ctrl: {e}")),
            log_tail: Vec::new(),
            duration_ms: started.elapsed().as_millis() as u64,
        },
    }
}

/// Run `rugix-ctrl` with the given arguments and return just the exit status.
async fn run_rugix_ctrl_status(args: &[&str]) -> DeviceCommandDoneData {
    let started = std::time::Instant::now();
    match tokio::process::Command::new("rugix-ctrl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let log_tail: Vec<String> = stderr.lines().map(|l| l.to_owned()).collect();
            let duration_ms = started.elapsed().as_millis() as u64;
            if output.status.success() {
                DeviceCommandDoneData {
                    status: DeviceCommandStatus::Ok,
                    output: None,
                    error: None,
                    log_tail,
                    duration_ms,
                }
            } else {
                DeviceCommandDoneData {
                    status: DeviceCommandStatus::Error,
                    output: None,
                    error: Some(format!("rugix-ctrl exited with status {}", output.status)),
                    log_tail,
                    duration_ms,
                }
            }
        }
        Err(e) => DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("failed to run rugix-ctrl: {e}")),
            log_tail: Vec::new(),
            duration_ms: started.elapsed().as_millis() as u64,
        },
    }
}

fn error_done(error: String) -> DeviceCommandDoneData {
    DeviceCommandDoneData {
        status: DeviceCommandStatus::Error,
        output: None,
        error: Some(error),
        log_tail: Vec::new(),
        duration_ms: 0,
    }
}

struct ListCommand;

impl BuiltinCommand for ListCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.rugix-apps.list".to_owned(),
            description: Some("List installed Rugix Apps".to_owned()),
            category: Some("applications".to_owned()),
            input: None,
            output: None,
        }
    }

    fn execute(
        &self,
        _ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(async { run_rugix_ctrl_json(&["apps", "list"]).await })
    }
}

struct InfoCommand;

impl BuiltinCommand for InfoCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.rugix-apps.info".to_owned(),
            description: Some("Get details of a Rugix App".to_owned()),
            category: Some("applications".to_owned()),
            input: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "app": {
                        "type": "string",
                        "description": "Name of the application"
                    }
                },
                "required": ["app"]
            })),
            output: None,
        }
    }

    fn execute(
        &self,
        ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(async move {
            let app = match extract_app(&ctx.input) {
                Ok(app) => app,
                Err(done) => return done,
            };
            run_rugix_ctrl_json(&["apps", "info", &app]).await
        })
    }
}

struct StartCommand;

impl BuiltinCommand for StartCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.rugix-apps.start".to_owned(),
            description: Some("Start a Rugix App".to_owned()),
            category: Some("applications".to_owned()),
            input: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "app": {
                        "type": "string",
                        "description": "Name of the application"
                    }
                },
                "required": ["app"]
            })),
            output: None,
        }
    }

    fn execute(
        &self,
        ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(async move {
            let app = match extract_app(&ctx.input) {
                Ok(app) => app,
                Err(done) => return done,
            };
            run_rugix_ctrl_status(&["apps", "start", &app]).await
        })
    }
}

struct StopCommand;

impl BuiltinCommand for StopCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.rugix-apps.stop".to_owned(),
            description: Some("Stop a Rugix App".to_owned()),
            category: Some("applications".to_owned()),
            input: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "app": {
                        "type": "string",
                        "description": "Name of the application"
                    }
                },
                "required": ["app"]
            })),
            output: None,
        }
    }

    fn execute(
        &self,
        ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(async move {
            let app = match extract_app(&ctx.input) {
                Ok(app) => app,
                Err(done) => return done,
            };
            run_rugix_ctrl_status(&["apps", "stop", &app]).await
        })
    }
}

struct RollbackCommand;

impl BuiltinCommand for RollbackCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.rugix-apps.rollback".to_owned(),
            description: Some("Rollback a Rugix App to its previous generation".to_owned()),
            category: Some("applications".to_owned()),
            input: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "app": {
                        "type": "string",
                        "description": "Name of the application"
                    }
                },
                "required": ["app"]
            })),
            output: None,
        }
    }

    fn execute(
        &self,
        ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(async move {
            let app = match extract_app(&ctx.input) {
                Ok(app) => app,
                Err(done) => return done,
            };
            run_rugix_ctrl_status(&["apps", "rollback", &app]).await
        })
    }
}

struct RemoveCommand;

impl BuiltinCommand for RemoveCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.rugix-apps.remove".to_owned(),
            description: Some("Remove a Rugix App".to_owned()),
            category: Some("applications".to_owned()),
            input: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "app": {
                        "type": "string",
                        "description": "Name of the application"
                    }
                },
                "required": ["app"]
            })),
            output: None,
        }
    }

    fn execute(
        &self,
        ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(async move {
            let app = match extract_app(&ctx.input) {
                Ok(app) => app,
                Err(done) => return done,
            };
            run_rugix_ctrl_status(&["apps", "remove", &app]).await
        })
    }
}

struct DeployCommand;

impl BuiltinCommand for DeployCommand {
    fn descriptor(&self) -> DeviceCommandDescriptor {
        DeviceCommandDescriptor {
            name: "nexigon.rugix-apps.deploy".to_owned(),
            description: Some("Deploy a Rugix App from a package version".to_owned()),
            category: Some("applications".to_owned()),
            input: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "versionId": {
                        "type": "string",
                        "description": "Package version ID to deploy"
                    },
                    "assetFilename": {
                        "type": "string",
                        "description": "Asset filename (defaults to first .rugixb asset)"
                    }
                },
                "required": ["versionId"]
            })),
            output: None,
        }
    }

    fn execute(
        &self,
        mut ctx: InvocationCtx,
    ) -> Pin<Box<dyn std::future::Future<Output = DeviceCommandDoneData> + Send + '_>> {
        Box::pin(async move {
            let started = std::time::Instant::now();

            let deploy_input = match extract_deploy_input(&ctx.input) {
                Ok(input) => input,
                Err(done) => return done,
            };

            // Obtain a hub executor to resolve the download URL.
            let Some(mut connection_ref) = ctx.connection_ref.take() else {
                return error_done("hub connection not available".to_owned());
            };
            let mut executor = match connect_executor(&mut connection_ref).await {
                Ok(executor) => executor,
                Err(e) => return error_done(format!("failed to connect executor: {e}")),
            };

            // Resolve the version details to find the .rugixb asset.
            let version_details = match executor
                .execute(GetPackageVersionDetailsAction::new(deploy_input.version_id))
                .await
            {
                Ok(Ok(details)) => details,
                Ok(Err(e)) => {
                    return error_done(format!("failed to get version details: {}", e.message));
                }
                Err(e) => return error_done(format!("executor error: {e}")),
            };

            // Find the bundle asset.
            let asset = if let Some(filename) = &deploy_input.asset_filename {
                version_details
                    .assets
                    .iter()
                    .find(|a| &a.filename == filename)
            } else {
                version_details
                    .assets
                    .iter()
                    .find(|a| a.filename.ends_with(".rugixb"))
            };
            let Some(asset) = asset else {
                return error_done("no .rugixb asset found in version".to_owned());
            };
            let asset_id = asset.asset_id.clone();

            // Issue a download URL.
            let download_url = match executor
                .execute(IssueAssetDownloadUrlAction::new(asset_id))
                .await
            {
                Ok(Ok(output)) => output.url,
                Ok(Err(e)) => {
                    return error_done(format!("failed to issue download URL: {}", e.message));
                }
                Err(e) => return error_done(format!("executor error: {e}")),
            };

            // Pass the download URL directly to rugix-ctrl.
            match tokio::process::Command::new("rugix-ctrl")
                .args(["apps", "install", &download_url])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await
            {
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let log_tail: Vec<String> = stderr.lines().map(|l| l.to_owned()).collect();
                    let duration_ms = started.elapsed().as_millis() as u64;
                    if output.status.success() {
                        let json_output =
                            serde_json::from_slice::<serde_json::Value>(&output.stdout).ok();
                        DeviceCommandDoneData {
                            status: DeviceCommandStatus::Ok,
                            output: json_output,
                            error: None,
                            log_tail,
                            duration_ms,
                        }
                    } else {
                        DeviceCommandDoneData {
                            status: DeviceCommandStatus::Error,
                            output: None,
                            error: Some(format!("rugix-ctrl exited with status {}", output.status)),
                            log_tail,
                            duration_ms,
                        }
                    }
                }
                Err(e) => DeviceCommandDoneData {
                    status: DeviceCommandStatus::Error,
                    output: None,
                    error: Some(format!("failed to wait for rugix-ctrl: {e}")),
                    log_tail: Vec::new(),
                    duration_ms: started.elapsed().as_millis() as u64,
                },
            }
        })
    }
}
