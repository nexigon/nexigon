//! On-demand command execution for device-side operations.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use nexigon_api::types::devices::DeviceCommandDeviceFrame;
use nexigon_api::types::devices::DeviceCommandDoneData;
use nexigon_api::types::devices::DeviceCommandHubFrame;
use nexigon_api::types::devices::DeviceCommandInvokeData;
use nexigon_api::types::devices::DeviceCommandLogData;
use nexigon_api::types::devices::DeviceCommandStatus;
use nexigon_api::types::properties::DeviceCommandDescriptor;
use nexigon_api::types::properties::DeviceCommandManifest;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::builtins::BuiltinCommand;
use crate::builtins::InvocationCtx;
use crate::config::CommandDefinition;
use crate::config::CommandSchemaBlock;
use crate::config::Config;
use crate::config::commands::CommandStdoutLine;

/// Maximum size of the stderr ring buffer in bytes.
const STDERR_TAIL_MAX_BYTES: usize = 8192;

const DEFAULT_COMMAND_TIMEOUT: u64 = 30;

/// A command in the registry, either external (TOML + script) or built-in.
pub enum RegisteredCommand {
    External(CommandDefinition),
    Builtin(Box<dyn BuiltinCommand>),
}

/// Registry of loaded command definitions.
pub struct CommandRegistry {
    commands: HashMap<String, RegisteredCommand>,
}

impl CommandRegistry {
    /// Load external command definitions from TOML files in the given directory.
    pub fn load_external(directory: &Path) -> anyhow::Result<Self> {
        let mut commands = HashMap::new();
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!(
                    ?directory,
                    "commands directory does not exist, no commands loaded"
                );
                return Ok(Self { commands });
            }
            Err(e) => return Err(e).context("failed to read commands directory"),
        };

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }

            // Skip world-writable files for security.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let metadata = std::fs::metadata(&path)?;
                let mode = metadata.permissions().mode();
                if mode & 0o002 != 0 {
                    error!(?path, "skipping world-writable command file");
                    continue;
                }
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    error!(?path, "failed to read command file: {e}");
                    continue;
                }
            };
            let def: CommandDefinition = match toml::from_str(&content) {
                Ok(d) => d,
                Err(e) => {
                    error!(?path, "failed to parse command file: {e}");
                    continue;
                }
            };
            info!(name = %def.command.name, ?path, "loaded command");
            let name = def.command.name.clone();
            commands.insert(name, RegisteredCommand::External(def));
        }

        info!(count = commands.len(), "loaded external commands");
        Ok(Self { commands })
    }

    /// Add built-in commands to the registry, skipping any that collide with
    /// already-registered external commands.
    pub fn add_builtins(&mut self, builtins: Vec<Box<dyn BuiltinCommand>>) {
        for builtin in builtins {
            let descriptor = builtin.descriptor();
            if self.commands.contains_key(&descriptor.name) {
                warn!(
                    name = %descriptor.name,
                    "skipping built-in command that collides with external command"
                );
                continue;
            }
            info!(name = %descriptor.name, "registered built-in command");
            let name = descriptor.name.clone();
            self.commands
                .insert(name, RegisteredCommand::Builtin(builtin));
        }
    }

    /// Get a command by name.
    pub fn get(&self, name: &str) -> Option<&RegisteredCommand> {
        self.commands.get(name)
    }

    /// Build the capability manifest for publishing as a device property.
    pub fn manifest(&self) -> DeviceCommandManifest {
        DeviceCommandManifest {
            commands: self
                .commands
                .values()
                .map(|cmd| match cmd {
                    RegisteredCommand::External(def) => {
                        let parse_schema =
                            |block: &Option<CommandSchemaBlock>| -> Option<serde_json::Value> {
                                block
                                    .as_ref()
                                    .and_then(|s| serde_json::from_str(&s.schema).ok())
                            };
                        DeviceCommandDescriptor {
                            name: def.command.name.clone(),
                            description: def.command.description.clone(),
                            category: def.command.category.clone(),
                            input: parse_schema(&def.input),
                            output: parse_schema(&def.output),
                        }
                    }
                    RegisteredCommand::Builtin(cmd) => cmd.descriptor(),
                })
                .collect(),
        }
    }
}

/// Handle a command invocation over a multiplex channel.
pub async fn handle_handler_channel(
    channel: nexigon_multiplex::Channel,
    _config: &Arc<Config>,
    registry: &Arc<CommandRegistry>,
) -> anyhow::Result<()> {
    let (mut chan_writer, mut chan_reader) = channel.split();

    // Read the Invoke frame.
    let DeviceCommandHubFrame::Invoke(request) = read_hub_frame(&mut chan_reader).await?;

    debug!(
        command = %request.command,
        stream_log = request.stream_log,
        "command invocation"
    );

    let Some(registered) = registry.get(&request.command) else {
        let frame = DeviceCommandDeviceFrame::Done(DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("command {:?} not found", request.command)),
            log_tail: Vec::new(),
            duration_ms: 0,
        });
        write_device_frame(&mut chan_writer, &frame).await?;
        return Ok(());
    };

    let done_frame = match registered {
        RegisteredCommand::External(command_def) => {
            execute_external_command(command_def, &request, &mut chan_writer).await?
        }
        RegisteredCommand::Builtin(cmd) => {
            let timeout_secs: Option<u64> = request.timeout_secs.map(|t| t.into());
            let ctx = InvocationCtx {
                input: request.input.clone(),
            };
            let started = std::time::Instant::now();
            let done = if let Some(timeout_secs) = timeout_secs {
                let timeout = std::time::Duration::from_secs(timeout_secs);
                match tokio::time::timeout(timeout, cmd.execute(ctx)).await {
                    Ok(done) => done,
                    Err(_) => DeviceCommandDoneData {
                        status: DeviceCommandStatus::Error,
                        output: None,
                        error: Some(format!("command timed out after {timeout_secs}s")),
                        log_tail: Vec::new(),
                        duration_ms: started.elapsed().as_millis() as u64,
                    },
                }
            } else {
                cmd.execute(ctx).await
            };
            DeviceCommandDeviceFrame::Done(done)
        }
    };

    write_device_frame(&mut chan_writer, &done_frame).await.ok();

    Ok(())
}

/// Execute an external (TOML-defined, subprocess-based) command.
async fn execute_external_command(
    command_def: &CommandDefinition,
    request: &DeviceCommandInvokeData,
    chan_writer: &mut (impl AsyncWriteExt + Unpin),
) -> anyhow::Result<DeviceCommandDeviceFrame> {
    let started = std::time::Instant::now();

    // Spawn the command process.
    let (program, args) = command_def
        .exec
        .handler
        .split_first()
        .context("handler must have at least one element")?;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn command {:?}", command_def.exec.handler))?;

    let stream_log = request.stream_log.unwrap_or(false);
    let mut child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();

    let write_stdin = async {
        if !request.input.is_null() {
            let mut line = serde_json::to_vec(&request.input).unwrap();
            line.push(b'\n');
            child_stdin.write_all(&line).await.ok();
        }
        drop(child_stdin);
    };

    let mut stderr_ring = StderrRingBuffer::new(STDERR_TAIL_MAX_BYTES);
    let mut stderr_reader = tokio::io::BufReader::new(child_stderr);
    let read_stderr = async {
        let mut line_buf = String::new();
        loop {
            line_buf.clear();
            match stderr_reader.read_line(&mut line_buf).await {
                Ok(0) => break,
                Ok(_) => {
                    stderr_ring.push(line_buf.as_bytes());
                    if stream_log {
                        let log_frame = DeviceCommandDeviceFrame::Log(DeviceCommandLogData {
                            lines: vec![line_buf.clone()],
                        });
                        if write_device_frame(chan_writer, &log_frame).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    debug!("stderr read error: {e}");
                    break;
                }
            }
        }
    };

    let mut stdout_reader = tokio::io::BufReader::new(child_stdout);
    let mut last_output: Option<serde_json::Value> = None;
    let read_stdout = async {
        let mut line_buf = String::new();
        loop {
            line_buf.clear();
            match stdout_reader.read_line(&mut line_buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line_buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    // Unknown types are silently ignored for forward compatibility.
                    if let Ok(line) = serde_json::from_str::<CommandStdoutLine>(trimmed) {
                        match line {
                            CommandStdoutLine::Output(output) => {
                                last_output = Some(output.data);
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!("stdout read error: {e}");
                    break;
                }
            }
        }
    };

    // Timeout: request timeout takes precedence, falls back to command config.
    // No timeout when streaming unless explicitly set.
    let timeout_secs: Option<u64> = match request.timeout_secs {
        Some(t) => Some(t.into()),
        None if stream_log => None,
        None => Some(command_def.exec.timeout.unwrap_or(DEFAULT_COMMAND_TIMEOUT)),
    };

    // Run stdin/stdout/stderr I/O and process wait concurrently, with optional timeout.
    let io_and_wait = async {
        tokio::join!(write_stdin, read_stdout, read_stderr);
        child.wait().await
    };

    let process_result = if let Some(timeout_secs) = timeout_secs {
        let timeout = std::time::Duration::from_secs(timeout_secs);
        tokio::select! {
            status = io_and_wait => Ok(status),
            _ = tokio::time::sleep(timeout) => {
                child.kill().await.ok();
                Err(timeout_secs)
            }
        }
    } else {
        Ok(io_and_wait.await)
    };

    let duration_ms = started.elapsed().as_millis() as u64;

    let log_tail = stderr_ring.into_lines();

    let done_frame = match process_result {
        Ok(Ok(exit_status)) if exit_status.success() => {
            DeviceCommandDeviceFrame::Done(DeviceCommandDoneData {
                status: DeviceCommandStatus::Ok,
                output: last_output,
                error: None,
                log_tail,
                duration_ms,
            })
        }
        Ok(Ok(exit_status)) => DeviceCommandDeviceFrame::Done(DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("command exited with status {exit_status}")),
            log_tail,
            duration_ms,
        }),
        Ok(Err(e)) => DeviceCommandDeviceFrame::Done(DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("failed to wait for command: {e}")),
            log_tail,
            duration_ms,
        }),
        Err(timeout_secs) => DeviceCommandDeviceFrame::Done(DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some(format!("command timed out after {timeout_secs}s")),
            log_tail,
            duration_ms,
        }),
    };

    Ok(done_frame)
}

/// Fixed-capacity ring buffer that retains the last N bytes.
struct StderrRingBuffer {
    buf: VecDeque<u8>,
    capacity: usize,
}

impl StderrRingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, data: &[u8]) {
        for &byte in data {
            if self.buf.len() == self.capacity {
                self.buf.pop_front();
            }
            self.buf.push_back(byte);
        }
    }

    fn into_lines(self) -> Vec<String> {
        let bytes: Vec<u8> = self.buf.into();
        let text = String::from_utf8_lossy(&bytes);
        text.lines().map(|l| l.to_owned()).collect()
    }
}

/// Read a hub frame from the channel.
async fn read_hub_frame(
    reader: &mut (impl AsyncReadExt + Unpin),
) -> anyhow::Result<DeviceCommandHubFrame> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .context("failed to read frame length")?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .await
        .context("failed to read frame payload")?;
    serde_json::from_slice(&buf).context("failed to deserialize hub frame")
}

/// Write a device frame to the channel.
async fn write_device_frame(
    writer: &mut (impl AsyncWriteExt + Unpin),
    frame: &DeviceCommandDeviceFrame,
) -> anyhow::Result<()> {
    let data = serde_json::to_vec(frame)?;
    let len = (data.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(&data).await?;
    writer.flush().await?;
    Ok(())
}
