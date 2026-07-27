use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::bail;
use nexigon_agent_protocol::HubTerminalFrame;
use nexigon_agent_protocol::read_hub_terminal_frame;
use nexigon_agent_protocol::write_terminal_data;
use nexigon_agent_protocol::write_terminal_exit;
use nix::libc;
use nix::unistd::User;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::config::Config;

mod child;

const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CHILD_TERMINATION_GRACE: Duration = Duration::from_secs(5);
const PTY_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Handle a terminal session over a multiplex channel.
///
/// The channel uses a length-prefixed binary framing protocol:
/// `[u32 BE: length][u8: type][payload]`
pub async fn handle_terminal_session(
    channel: nexigon_multiplex::Channel,
    config: &Arc<Config>,
    requested_user: Option<&str>,
) -> anyhow::Result<()> {
    handle_terminal_session_with_cancellation(
        channel,
        config,
        requested_user,
        CancellationToken::new(),
    )
    .await
}

/// Handle a terminal session that is cancelled with its owning connection.
pub(crate) async fn handle_terminal_session_with_cancellation(
    channel: nexigon_multiplex::Channel,
    config: &Arc<Config>,
    requested_user: Option<&str>,
    cancellation: CancellationToken,
) -> anyhow::Result<()> {
    if cancellation.is_cancelled() {
        return Ok(());
    }
    if !crate::config::terminal_enabled(config) {
        bail!("terminal is not enabled or terminal.user is invalid");
    }
    let terminal_config = config
        .terminal
        .as_ref()
        .context("terminal configuration is missing")?;
    let default_user = crate::config::terminal_user(config).context("terminal.user is invalid")?;

    let username = requested_user.unwrap_or(default_user);

    let allowed_users = terminal_config.allowed_users.as_ref();
    match allowed_users {
        Some(allowed) => {
            if !allowed.iter().any(|u| u == username) {
                bail!("user {username:?} is not in the allowed users list");
            }
        }
        None => {
            if username != default_user {
                bail!(
                    "user {username:?} is not allowed (only the default user {default_user:?} is permitted; \
                     configure `allowed-users` to allow additional users)"
                );
            }
        }
    }

    let user = User::from_name(username)
        .context("failed to look up user")?
        .with_context(|| format!("user {username:?} does not exist"))?;

    let shell = terminal_config
        .shell
        .as_deref()
        .map(|s| s.to_owned())
        .unwrap_or_else(|| {
            let login_shell = user.shell.to_string_lossy().to_string();
            if login_shell.is_empty() {
                "/bin/sh".to_owned()
            } else {
                login_shell
            }
        });

    info!(username, shell, "spawning terminal session");

    let shell_name = std::path::Path::new(&shell)
        .file_name()
        .unwrap_or(std::ffi::OsStr::new("sh"))
        .to_string_lossy()
        .to_string();
    let login_shell_name = format!("-{shell_name}");
    let mut prepared_child = child::prepare(&user, &shell, &login_shell_name)
        .context("failed to prepare terminal child")?;

    // SAFETY: We only call async-signal-safe functions in the child.
    let forkpty_result = unsafe { nix::pty::forkpty(None, None) }.context("failed to forkpty")?;

    match forkpty_result {
        nix::pty::ForkptyResult::Child => {
            // SAFETY: All allocating/NSS work and pointer construction happened
            // before `forkpty`. `enter` uses only raw Linux syscalls and `_exit`.
            unsafe { child::enter(&mut prepared_child) }
        }
        nix::pty::ForkptyResult::Parent { child, master } => {
            // Raw pointer arrays in the child preparation are not needed by the
            // parent and must not remain live across any async suspension point.
            drop(prepared_child);
            let parent_setup = (|| {
                let master = File::from(master);
                let master_fd = master.as_raw_fd();
                // AsyncFd requires non-blocking mode.
                set_nonblocking(master_fd).context("failed to set PTY master to non-blocking")?;
                let async_master_read = tokio::io::unix::AsyncFd::new(master.try_clone()?)
                    .context("failed to register PTY reader")?;
                let async_master_write = tokio::io::unix::AsyncFd::new(master)
                    .context("failed to register PTY writer")?;
                Ok::<_, anyhow::Error>((master_fd, async_master_read, async_master_write))
            })();
            let (master_fd, async_master_read, async_master_write) = match parent_setup {
                Ok(setup) => setup,
                Err(error) => {
                    terminate_terminal_child(child).await;
                    return Err(error).context("failed to initialize terminal parent");
                }
            };
            let (mut chan_writer, mut chan_reader) = channel.split();

            let channel_to_pty = async {
                loop {
                    match read_hub_terminal_frame(&mut chan_reader).await? {
                        HubTerminalFrame::Data(data) => {
                            pty_write(&async_master_write, &data).await?;
                        }
                        HubTerminalFrame::Resize { cols, rows } => {
                            let ws = nix::pty::Winsize {
                                ws_row: rows,
                                ws_col: cols,
                                ws_xpixel: 0,
                                ws_ypixel: 0,
                            };
                            // SAFETY: `master_fd` is a live PTY master and `ws` is a
                            // correctly initialized `winsize` value.
                            let result = unsafe {
                                libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws as *const _)
                            };
                            if result < 0 {
                                return Err(std::io::Error::last_os_error())
                                    .context("failed to resize PTY");
                            }
                        }
                    }
                }
            };

            let pty_to_channel = async {
                let mut buf = vec![0u8; 4096];
                loop {
                    let n = pty_read(&async_master_read, &mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    write_terminal_data(&mut chan_writer, &buf[..n]).await?;
                }
                Ok::<(), anyhow::Error>(())
            };

            let mut channel_to_pty = Box::pin(channel_to_pty);
            let mut pty_to_channel = Box::pin(pty_to_channel);
            let mut wait_child = Box::pin(wait_for_terminal_child(child));

            enum SessionEnd {
                Channel(anyhow::Result<()>),
                Pty(anyhow::Result<()>),
                Child(anyhow::Result<i32>),
                Cancelled,
            }

            let end = tokio::select! {
                biased;
                () = cancellation.cancelled() => SessionEnd::Cancelled,
                result = &mut wait_child => SessionEnd::Child(result),
                result = &mut pty_to_channel => SessionEnd::Pty(result),
                result = &mut channel_to_pty => SessionEnd::Channel(result),
            };

            let exit_code = match end {
                SessionEnd::Child(result) => {
                    let code = result.context("failed to reap terminal child")?;
                    debug!(code, "terminal child exited");
                    // Preserve all output that was already in the PTY before publishing
                    // the final status. The bounded grace prevents a broken PTY from
                    // delaying channel closure indefinitely.
                    match tokio::time::timeout(PTY_DRAIN_GRACE, &mut pty_to_channel).await {
                        Ok(Ok(())) => code,
                        Ok(Err(error)) => {
                            drop(pty_to_channel);
                            drop(channel_to_pty);
                            drop(wait_child);
                            chan_writer.shutdown().await.ok();
                            return Err(error).context("failed to drain terminal output");
                        }
                        Err(_) => {
                            // The canceled write may already have emitted part of a data
                            // frame. Close instead of appending an exit frame to a stream
                            // whose alignment is no longer known.
                            drop(pty_to_channel);
                            drop(channel_to_pty);
                            drop(wait_child);
                            chan_writer.shutdown().await.ok();
                            anyhow::bail!("timed out draining terminal output");
                        }
                    }
                }
                SessionEnd::Pty(Ok(())) => {
                    // Linux can report PTY EOF immediately before waitpid exposes the
                    // status. Wait for that status instead of inventing a successful exit.
                    match tokio::time::timeout(CHILD_TERMINATION_GRACE, &mut wait_child).await {
                        Ok(result) => result.context("failed to reap terminal child")?,
                        Err(_) => terminate_terminal_child(child).await,
                    }
                }
                SessionEnd::Pty(Err(error)) => {
                    debug!(?error, "PTY output relay failed; closing terminal channel");
                    drop(pty_to_channel);
                    drop(channel_to_pty);
                    drop(wait_child);
                    terminate_terminal_child(child).await;
                    chan_writer.shutdown().await.ok();
                    return Err(error);
                }
                SessionEnd::Channel(error) => {
                    debug!(?error, "invalid or closed terminal input; closing channel");
                    drop(pty_to_channel);
                    drop(channel_to_pty);
                    drop(wait_child);
                    terminate_terminal_child(child).await;
                    chan_writer.shutdown().await.ok();
                    return error;
                }
                SessionEnd::Cancelled => {
                    debug!("terminal session cancelled; terminating child");
                    drop(pty_to_channel);
                    drop(channel_to_pty);
                    drop(wait_child);
                    terminate_terminal_child(child).await;
                    chan_writer.shutdown().await.ok();
                    return Ok(());
                }
            };

            // A terminal leader can exit while a non-interactive background descendant keeps
            // running. The forkpty child owns this process group, so close the entire session
            // before reporting completion.
            nix::sys::signal::killpg(child, nix::sys::signal::Signal::SIGKILL).ok();

            // Dropping all relay/wait futures releases their mutable borrows before the
            // one and only terminal status frame is sent.
            drop(pty_to_channel);
            drop(channel_to_pty);
            drop(wait_child);
            write_terminal_exit(&mut chan_writer, exit_code).await?;
            chan_writer.shutdown().await?;
            Ok(())
        }
    }
}

async fn wait_for_terminal_child(child: nix::unistd::Pid) -> anyhow::Result<i32> {
    loop {
        match nix::sys::wait::waitpid(child, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => return Ok(code),
            Ok(nix::sys::wait::WaitStatus::Signaled(_, signal, _)) => {
                return Ok(128 + signal as i32);
            }
            Ok(nix::sys::wait::WaitStatus::StillAlive)
            | Ok(nix::sys::wait::WaitStatus::Stopped(_, _))
            | Ok(nix::sys::wait::WaitStatus::Continued(_))
            | Ok(nix::sys::wait::WaitStatus::PtraceEvent(_, _, _))
            | Ok(nix::sys::wait::WaitStatus::PtraceSyscall(_)) => {
                tokio::time::sleep(CHILD_POLL_INTERVAL).await;
            }
            Err(error) => return Err(error).context("waitpid failed"),
        }
    }
}

async fn terminate_terminal_child(child: nix::unistd::Pid) -> i32 {
    signal_terminal_process_group(child, nix::sys::signal::Signal::SIGTERM);
    match tokio::time::timeout(CHILD_TERMINATION_GRACE, wait_for_terminal_child(child)).await {
        Ok(Ok(code)) => {
            // The shell may exit while a descendant ignores SIGTERM. Ensure nothing in
            // the terminal's process group survives after the leader has been reaped.
            nix::sys::signal::killpg(child, nix::sys::signal::Signal::SIGKILL).ok();
            code
        }
        Ok(Err(error)) => {
            debug!(?error, "failed to reap terminal child after SIGTERM");
            nix::sys::signal::killpg(child, nix::sys::signal::Signal::SIGKILL).ok();
            255
        }
        Err(_) => {
            warn!("terminal process group did not exit after SIGTERM, sending SIGKILL");
            signal_terminal_process_group(child, nix::sys::signal::Signal::SIGKILL);
            wait_for_terminal_child(child).await.unwrap_or(255)
        }
    }
}

fn signal_terminal_process_group(child: nix::unistd::Pid, signal: nix::sys::signal::Signal) {
    if nix::sys::signal::killpg(child, signal).is_err() {
        nix::sys::signal::kill(child, signal).ok();
    }
}

/// Read from a PTY master fd asynchronously.
async fn pty_read(fd: &tokio::io::unix::AsyncFd<File>, buf: &mut [u8]) -> anyhow::Result<usize> {
    match fd
        .async_io(tokio::io::Interest::READABLE, |mut f| f.read(buf))
        .await
    {
        Ok(read) => Ok(read),
        // Linux PTY masters report EIO once the final slave closes; this is EOF,
        // not a relay failure.
        Err(error) if error.raw_os_error() == Some(libc::EIO) => Ok(0),
        Err(error) => Err(error.into()),
    }
}

/// Write to a PTY master fd asynchronously.
async fn pty_write(fd: &tokio::io::unix::AsyncFd<File>, buf: &[u8]) -> anyhow::Result<()> {
    let mut written = 0;
    while written < buf.len() {
        written += fd
            .async_io(tokio::io::Interest::WRITABLE, |mut f| {
                f.write(&buf[written..])
            })
            .await?;
    }
    Ok(())
}

/// Set a file descriptor to non-blocking mode.
fn set_nonblocking(fd: std::os::fd::RawFd) -> anyhow::Result<()> {
    let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL).context("fcntl F_GETFL")?;
    nix::fcntl::fcntl(
        fd,
        nix::fcntl::FcntlArg::F_SETFL(
            nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK,
        ),
    )
    .context("fcntl F_SETFL O_NONBLOCK")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use bytes::Bytes;
    use futures::StreamExt;
    use nexigon_agent_protocol::DeviceTerminalFrame;
    use nexigon_agent_protocol::read_device_terminal_frame;
    use nexigon_agent_protocol::write_terminal_data;
    use nexigon_multiplex::Connection;
    use nexigon_multiplex::ConnectionEvent;
    use nexigon_multiplex::transport::InMemory;
    use nix::unistd::User;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::config::TerminalConfig;

    async fn assert_process_gone(description: &str, pid: i32) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
                    Err(nix::errno::Errno::ESRCH) => break,
                    Ok(()) | Err(nix::errno::Errno::EPERM) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("unexpected error checking {description}: {error}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{description} still exists after terminal cleanup"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_terminal_exits_report_the_real_status_exactly_once() {
        let (hub_transport, agent_transport) = InMemory::<Bytes, Bytes>::new_buffered(64);
        let mut hub_connection = Connection::new(hub_transport);
        let mut agent_connection = Connection::new(agent_transport);
        let mut hub_ref = hub_connection.make_ref();
        let (channel_tx, mut channel_rx) = tokio::sync::mpsc::unbounded_channel();

        let hub_driver = tokio::spawn(async move {
            while let Some(event) = hub_connection.next().await {
                if !matches!(event, Ok(ConnectionEvent::Connected)) {
                    break;
                }
            }
        });
        let agent_driver = tokio::spawn(async move {
            while let Some(event) = agent_connection.next().await {
                match event {
                    Ok(ConnectionEvent::Connected) => {}
                    Ok(ConnectionEvent::RequestChannel(request)) => {
                        let channel_tx = channel_tx.clone();
                        request.accept(move |channel| {
                            channel_tx.send(channel).unwrap();
                        });
                    }
                    Ok(ConnectionEvent::Closed) | Err(_) => break,
                }
            }
        });

        let user = User::from_uid(nix::unistd::geteuid())
            .unwrap()
            .expect("current user must exist");
        let config = Arc::new(
            Config::new(PathBuf::from("unused-fingerprint")).with_terminal(Some(
                TerminalConfig::new()
                    .with_enabled(Some(true))
                    .with_user(Some(user.name))
                    .with_shell(Some("/bin/sh".to_owned())),
            )),
        );

        for iteration in 0..8 {
            let mut hub_channel =
                tokio::time::timeout(Duration::from_secs(5), hub_ref.open(b"terminal"))
                    .await
                    .expect("opening terminal channel timed out")
                    .expect("terminal channel rejected");
            let agent_channel = tokio::time::timeout(Duration::from_secs(5), channel_rx.recv())
                .await
                .expect("accepting terminal channel timed out")
                .expect("agent driver stopped");
            let session_config = config.clone();
            let session = tokio::spawn(async move {
                handle_terminal_session(agent_channel, &session_config, None).await
            });

            write_terminal_data(&mut hub_channel, b"exit 7\n")
                .await
                .unwrap();
            let exit = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match read_device_terminal_frame(&mut hub_channel).await.unwrap() {
                        DeviceTerminalFrame::Data(_) => {}
                        DeviceTerminalFrame::Exit(code) => break code,
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("terminal session {iteration} did not exit"));
            assert_eq!(exit, 7, "terminal session {iteration}");

            let after_exit = tokio::time::timeout(
                Duration::from_secs(2),
                read_device_terminal_frame(&mut hub_channel),
            )
            .await
            .expect("channel did not close after exit");
            assert!(after_exit.is_err(), "a second terminal frame followed exit");
            tokio::time::timeout(Duration::from_secs(2), session)
                .await
                .expect("terminal handler did not finish")
                .expect("terminal handler panicked")
                .expect("terminal handler failed");
        }

        // A malformed fixed-size control frame poisons the stream. Even if a valid
        // frame follows immediately, the handler must close rather than interpreting
        // the oversized control payload as another header.
        let mut hub_channel =
            tokio::time::timeout(Duration::from_secs(5), hub_ref.open(b"terminal"))
                .await
                .expect("opening malformed terminal channel timed out")
                .expect("malformed terminal channel rejected");
        let agent_channel = tokio::time::timeout(Duration::from_secs(5), channel_rx.recv())
            .await
            .expect("accepting malformed terminal channel timed out")
            .expect("agent driver stopped");
        let session_config = config.clone();
        let session = tokio::spawn(async move {
            handle_terminal_session(agent_channel, &session_config, None).await
        });
        hub_channel.write_all(&6u32.to_be_bytes()).await.unwrap();
        hub_channel
            .write_all(&[0x01, 0, 80, 0, 24, 0xff])
            .await
            .unwrap();
        write_terminal_data(&mut hub_channel, b"exit 7\n")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_secs(7),
                read_device_terminal_frame(&mut hub_channel),
            )
            .await
            .expect("malformed channel did not close")
            .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(7), session)
                .await
                .expect("malformed terminal handler did not finish")
                .expect("malformed terminal handler panicked")
                .is_err()
        );

        hub_driver.abort();
        agent_driver.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_children_are_reaped_on_cancellation_and_normal_exit() {
        let (hub_transport, agent_transport) = InMemory::<Bytes, Bytes>::new_buffered(64);
        let mut hub_connection = Connection::new(hub_transport);
        let mut agent_connection = Connection::new(agent_transport);
        let mut hub_ref = hub_connection.make_ref();
        let (channel_tx, mut channel_rx) = tokio::sync::mpsc::channel(1);

        let hub_driver = tokio::spawn(async move {
            while let Some(event) = hub_connection.next().await {
                if !matches!(event, Ok(ConnectionEvent::Connected)) {
                    break;
                }
            }
        });
        let agent_driver = tokio::spawn(async move {
            while let Some(event) = agent_connection.next().await {
                match event {
                    Ok(ConnectionEvent::Connected) => {}
                    Ok(ConnectionEvent::RequestChannel(request)) => {
                        let channel_tx = channel_tx.clone();
                        request.accept(move |channel| {
                            let _ = channel_tx.try_send(channel);
                        });
                    }
                    Ok(ConnectionEvent::Closed) | Err(_) => break,
                }
            }
        });

        let root = tempdir().unwrap();
        let pid_path = root.path().join("terminal-child.pid");
        let shell_path = root.path().join("terminal-shell");
        std::fs::write(
            &shell_path,
            format!(
                "#!/bin/sh\n/bin/sleep 60 & descendant=$!\necho \"$$ $descendant\" > '{}'\nwait\n",
                pid_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shell_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let user = User::from_uid(nix::unistd::geteuid())
            .unwrap()
            .expect("current user must exist");
        let config = Arc::new(
            Config::new(PathBuf::from("unused-fingerprint")).with_terminal(Some(
                TerminalConfig::new()
                    .with_enabled(Some(true))
                    .with_user(Some(user.name))
                    .with_shell(Some(shell_path.to_string_lossy().into_owned())),
            )),
        );

        let _hub_channel = hub_ref
            .open(b"terminal")
            .await
            .expect("terminal channel rejected");
        let agent_channel = channel_rx.recv().await.expect("agent driver stopped");
        let cancellation = CancellationToken::new();
        let session_cancellation = cancellation.clone();
        let session = tokio::spawn(async move {
            handle_terminal_session_with_cancellation(
                agent_channel,
                &config,
                None,
                session_cancellation,
            )
            .await
        });

        let (parent_pid, descendant_pid) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(raw) = tokio::fs::read_to_string(&pid_path).await {
                    let mut fields = raw.split_whitespace().map(str::parse::<i32>);
                    if let (Some(Ok(parent)), Some(Ok(descendant)), None) =
                        (fields.next(), fields.next(), fields.next())
                    {
                        break (parent, descendant);
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("terminal child did not start");

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(7), session)
            .await
            .expect("terminal session did not stop")
            .expect("terminal session panicked")
            .expect("terminal session failed");
        for (description, pid) in [
            ("terminal parent", parent_pid),
            ("terminal descendant", descendant_pid),
        ] {
            assert_process_gone(description, pid).await;
        }

        let completed_pid_path = root.path().join("completed-terminal-child.pid");
        let completed_shell_path = root.path().join("completed-terminal-shell");
        std::fs::write(
            &completed_shell_path,
            format!(
                "#!/bin/sh\n/bin/sleep 60 </dev/null >/dev/null 2>&1 & descendant=$!\necho \"$$ $descendant\" > '{}'\nexit 0\n",
                completed_pid_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            &completed_shell_path,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let user = User::from_uid(nix::unistd::geteuid())
            .unwrap()
            .expect("current user must exist");
        let completed_config = Arc::new(
            Config::new(PathBuf::from("unused-fingerprint")).with_terminal(Some(
                TerminalConfig::new()
                    .with_enabled(Some(true))
                    .with_user(Some(user.name))
                    .with_shell(Some(completed_shell_path.to_string_lossy().into_owned())),
            )),
        );
        let _hub_channel = hub_ref
            .open(b"terminal")
            .await
            .expect("completed terminal channel rejected");
        let agent_channel = channel_rx.recv().await.expect("agent driver stopped");
        let completed_session = tokio::spawn(async move {
            handle_terminal_session(agent_channel, &completed_config, None).await
        });
        tokio::time::timeout(Duration::from_secs(7), completed_session)
            .await
            .expect("completed terminal session did not stop")
            .expect("completed terminal session panicked")
            .expect("completed terminal session failed");
        let raw = std::fs::read_to_string(completed_pid_path).unwrap();
        let mut fields = raw.split_whitespace().map(str::parse::<i32>);
        let (Some(Ok(completed_parent)), Some(Ok(completed_descendant)), None) =
            (fields.next(), fields.next(), fields.next())
        else {
            panic!("completed terminal PID record was malformed");
        };
        for (description, pid) in [
            ("completed terminal parent", completed_parent),
            ("completed terminal descendant", completed_descendant),
        ] {
            assert_process_gone(description, pid).await;
        }

        hub_driver.abort();
        agent_driver.abort();
        let _ = hub_driver.await;
        let _ = agent_driver.await;
    }
}
