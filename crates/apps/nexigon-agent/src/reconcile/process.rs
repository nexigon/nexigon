//! Running the tools that change the device, such as Rugix Ctrl.

use anyhow::Context;
use anyhow::bail;
use tokio::process::Command;

/// Run a program and return its standard output.
///
/// Fails with the end of its standard error output if the program fails. The
/// error omits URLs among the arguments, as they may grant access to a download.
pub(crate) async fn run(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(program)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to run {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr.trim().lines().rev().take(5).collect::<Vec<_>>();
        let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        let args = args
            .iter()
            .map(|arg| {
                if arg.starts_with("http://") || arg.starts_with("https://") {
                    "<url>"
                } else {
                    arg
                }
            })
            .collect::<Vec<_>>();
        bail!(
            "`{program} {}` failed ({}): {tail}",
            args.join(" "),
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
