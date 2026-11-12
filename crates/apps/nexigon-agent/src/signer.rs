//! Device keys used through a signer script.
//!
//! The signer script performs operations with a device private key held by
//! hardware, for example a TPM, a PKCS#11 token, or device firmware. The agent
//! runs the script with one of the following operations as its only argument:
//!
//! - `public-key`: Write the DER-encoded subject public key info to stdout.
//! - `sign`: Hash and sign the message on stdin and write the DER-encoded ECDSA signature
//!   to stdout.
//!
//! A non-zero exit status fails the operation, and the script's stderr becomes
//! part of the error. [`load_signer_key`] returns the key as an [`ExternalKey`].

use std::fmt;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::bail;
use nexigon_cert::ExternalKey;
use nexigon_cert::SignError;
use nexigon_cert::Signer;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::config::SignerConfig;
use crate::handlers::kill_command_process_group;

/// Default seconds after which a signer operation fails.
const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// Load the device key held by the configured signer script.
pub(crate) async fn load_signer_key(
    config: &SignerConfig,
    config_dir: &Path,
) -> anyhow::Result<ExternalKey> {
    let timeout_secs = config.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    if timeout_secs == 0 {
        bail!("signer timeout-secs must be at least 1");
    }
    let script = SignerScript {
        path: config_dir.join(&config.script),
        timeout: Duration::from_secs(timeout_secs),
    };
    let public_key = script.run(Operation::PublicKey, &[]).await?;
    ExternalKey::new(&public_key, Arc::new(script))
        .context("signer script returned an invalid public key")
}

/// Signer script with its operation timeout.
#[derive(Debug)]
struct SignerScript {
    path: PathBuf,
    timeout: Duration,
}

impl SignerScript {
    /// Run an operation with the given stdin and return its stdout.
    #[tracing::instrument(level = tracing::Level::DEBUG, skip(self, input), fields(script = %self.path.display()))]
    async fn run(&self, operation: Operation, input: &[u8]) -> Result<Vec<u8>, SignerScriptError> {
        let path = || self.path.clone();
        let mut command = tokio::process::Command::new(&self.path);
        command
            .arg(operation.argument())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A timeout must also stop the tools that the script runs, which may
        // otherwise keep holding the hardware.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|source| SignerScriptError::Run {
            path: path(),
            source,
        })?;
        let process_group = child.id().and_then(|id| i32::try_from(id).ok());
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let write_input = async move { stdin.write_all(input).await };
        let (written, output) = tokio::time::timeout(self.timeout, async {
            tokio::join!(write_input, child.wait_with_output())
        })
        .await
        .map_err(|_| {
            kill_command_process_group(process_group);
            SignerScriptError::Timeout {
                path: path(),
                operation,
                timeout: self.timeout,
            }
        })?;
        let output = output.map_err(|source| SignerScriptError::Run {
            path: path(),
            source,
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SignerScriptError::Failed {
                path: path(),
                operation,
                status: output.status,
                stderr: match stderr.trim() {
                    "" => String::new(),
                    stderr => format!(": {stderr}"),
                },
            });
        }
        // The exit status and stderr explain a failed write better than the
        // write error, so they take precedence.
        written.map_err(|source| SignerScriptError::Input {
            path: path(),
            source,
        })?;
        Ok(output.stdout)
    }
}

impl Signer for SignerScript {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, SignError> {
        // Rustls and rcgen sign synchronously, usually on a Tokio worker thread
        // where blocking on a runtime panics. A dedicated thread with its own
        // runtime keeps the timeout enforceable.
        let sign = || {
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|source| SignerScriptError::Run {
                                path: self.path.clone(),
                                source,
                            })?
                            .block_on(self.run(Operation::Sign, message))
                    })
                    .join()
                    .expect("signer thread does not panic")
            })
        };
        // Waiting for the thread blocks the calling worker. On a multi-thread
        // runtime, other tasks move to another worker meanwhile.
        let signature =
            match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
                Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(sign),
                _ => sign(),
            }?;
        Ok(signature)
    }
}

/// Operation of the signer protocol.
#[derive(Debug, Clone, Copy)]
enum Operation {
    PublicKey,
    Sign,
}

impl Operation {
    /// Argument that selects the operation.
    fn argument(self) -> &'static str {
        match self {
            Self::PublicKey => "public-key",
            Self::Sign => "sign",
        }
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.argument())
    }
}

/// Failed signer operation.
#[derive(Debug, Error)]
enum SignerScriptError {
    #[error("failed to run signer script {}", path.display())]
    Run {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "signer script {} timed out after {} seconds in operation `{operation}`",
        path.display(),
        timeout.as_secs()
    )]
    Timeout {
        path: PathBuf,
        operation: Operation,
        timeout: Duration,
    },
    /// The script exited unsuccessfully, `stderr` is empty or starts with `: `.
    #[error("signer script {} failed operation `{operation}` with {status}{stderr}", path.display())]
    Failed {
        path: PathBuf,
        operation: Operation,
        status: ExitStatus,
        stderr: String,
    },
    #[error("failed to pass the message to signer script {}", path.display())]
    Input {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}
