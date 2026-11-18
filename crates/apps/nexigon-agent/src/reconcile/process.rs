//! Running the tools that change the device, such as Rugix Ctrl.

use std::time::Duration;

use anyhow::Context;
use anyhow::bail;
use tokio::process::Command;

/// Time after which a tool fails, unless it installs a bundle.
const TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Run a program and return its standard output.
///
/// Fails with the end of its standard error output if the program fails, and
/// after 10 minutes. The error omits URLs among the arguments, as they may
/// grant access to a download. Dropping the returned future kills the program.
pub(crate) async fn run(program: &str, args: &[&str]) -> anyhow::Result<String> {
    run_with_timeout(program, args, Some(TIMEOUT)).await
}

/// Run a program that installs a bundle like [`run`], but without a timeout.
///
/// The installation includes the download, which may take hours on a slow
/// connection. The tool's own network timeouts end a stalled download.
pub(crate) async fn run_install(program: &str, args: &[&str]) -> anyhow::Result<String> {
    run_with_timeout(program, args, None).await
}

/// Run a program like [`run`], but fail after `timeout`, if any.
async fn run_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Option<Duration>,
) -> anyhow::Result<String> {
    let args_display = args
        .iter()
        .map(|arg| {
            if arg.starts_with("http://") || arg.starts_with("https://") {
                "<url>"
            } else {
                arg
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let output = Command::new(program).args(args).kill_on_drop(true).output();
    let output = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, output).await.map_err(|_| {
            anyhow::anyhow!(
                "`{program} {args_display}` did not finish within {} seconds",
                timeout.as_secs()
            )
        })?,
        None => output.await,
    }
    .with_context(|| format!("failed to run {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr.trim().lines().rev().take(5).collect::<Vec<_>>();
        let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        bail!(
            "`{program} {args_display}` failed ({}): {tail}",
            output.status
        );
    }
    String::from_utf8(output.stdout).with_context(|| format!("{program} printed invalid UTF-8"))
}

/// Extract the version from output like `rugix-ctrl v1.3.0` or `rauc 1.15.2`.
pub(crate) fn tool_version(output: &str) -> String {
    let version = output.split_whitespace().last().unwrap_or_default();
    version.strip_prefix('v').unwrap_or(version).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program that does not finish in time is killed, and the error omits URLs.
    #[tokio::test]
    async fn run_fails_after_timeout() {
        let error = run_with_timeout(
            "sh",
            &["-c", "sleep 10", "https://example.com/bundle"],
            Some(Duration::from_millis(50)),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("did not finish"), "{error}");
        assert!(!error.contains("example.com"), "{error}");
    }
}
