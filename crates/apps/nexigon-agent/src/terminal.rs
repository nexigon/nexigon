use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use anyhow::Context;
use anyhow::bail;
use nix::libc;
use nix::pty::OpenptyResult;
use nix::sys::signal::Signal;
use nix::sys::signal::{self};
use nix::sys::wait::WaitPidFlag;
use nix::sys::wait::waitpid;
use nix::unistd::ForkResult;
use nix::unistd::Pid;
use nix::unistd::User;
use nix::unistd::fork;
use nix::unistd::setsid;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tracing::debug;
use tracing::warn;

use crate::config::Config;

/// Message types for the terminal framing protocol.
const MSG_DATA: u8 = 0x00;
const MSG_RESIZE: u8 = 0x01;
const MSG_EXIT: u8 = 0x02;

/// Handle a terminal session over a multiplex channel.
///
/// The channel uses a length-prefixed binary framing protocol:
/// `[u32 BE: length][u8: type][payload]`
pub async fn handle_terminal_session(
    channel: nexigon_multiplex::Channel,
    config: &Arc<Config>,
    requested_user: Option<&str>,
) -> anyhow::Result<()> {
    let terminal_config = config.terminal.as_ref();

    let default_user = terminal_config
        .and_then(|t| t.user.as_deref())
        .unwrap_or("root");

    let username = requested_user.unwrap_or(default_user);

    let allowed_users = terminal_config.and_then(|t| t.allowed_users.as_ref());
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
        .and_then(|t| t.shell.as_deref())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| {
            let login_shell = user.shell.to_string_lossy().to_string();
            if login_shell.is_empty() {
                "/bin/sh".to_owned()
            } else {
                login_shell
            }
        });

    debug!(username, shell, "spawning terminal session");

    let pty = nix::pty::openpty(None, None).context("failed to open PTY")?;
    let OpenptyResult { master, slave } = pty;

    let shell_name = std::path::Path::new(&shell)
        .file_name()
        .unwrap_or(std::ffi::OsStr::new("sh"))
        .to_string_lossy()
        .to_string();

    // These functions allocate and must thus happen prior to the fork.
    let login_shell_name = format!("-{shell_name}");
    let c_shell = std::ffi::CString::new(shell.as_str()).unwrap();
    let c_arg0 = std::ffi::CString::new(login_shell_name.as_str()).unwrap();

    // Fork the process.
    // SAFETY: We are careful to only call async-signal-safe functions in the child.
    let fork_result = unsafe { fork() }.context("failed to fork")?;

    match fork_result {
        ForkResult::Child => {
            drop(master);

            setsid().ok();

            unsafe {
                libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY, 0);
            }

            let slave_fd = slave.as_raw_fd();
            nix::unistd::dup2(slave_fd, 0).ok();
            nix::unistd::dup2(slave_fd, 1).ok();
            nix::unistd::dup2(slave_fd, 2).ok();
            if slave_fd > 2 {
                drop(slave);
            }

            let current_uid = nix::unistd::getuid();
            if current_uid.is_root() && !user.uid.is_root() {
                nix::unistd::setgroups(&[user.gid]).ok();
                nix::unistd::setgid(user.gid).ok();
                nix::unistd::setuid(user.uid).ok();
            }

            // SAFETY: We are in a forked child process with a single thread.
            unsafe {
                std::env::set_var("HOME", &user.dir);
                std::env::set_var("USER", &user.name);
                std::env::set_var("LOGNAME", &user.name);
                std::env::set_var("SHELL", &shell);
                std::env::set_var("TERM", "xterm-256color");
            }

            std::env::set_current_dir(&user.dir).ok();

            nix::unistd::execv(&c_shell, &[c_arg0]).ok();

            unsafe { libc::_exit(127) };
        }
        ForkResult::Parent { child } => {
            drop(slave);

            let result = run_session_relay(channel, File::from(master), child).await;

            cleanup_child(child).await;

            result
        }
    }
}

/// Run the relay between the multiplex channel and the PTY master fd.
async fn run_session_relay(
    channel: nexigon_multiplex::Channel,
    master: File,
    child: Pid,
) -> anyhow::Result<()> {
    let master_fd = master.as_raw_fd();

    let async_master_read = tokio::io::unix::AsyncFd::new(master.try_clone()?)?;
    let async_master_write = tokio::io::unix::AsyncFd::new(master)?;

    let (mut chan_reader, mut chan_writer) = tokio::io::split(channel);

    let mut read_buf = vec![0u8; 4096];

    let channel_to_pty = async {
        loop {
            let mut len_buf = [0u8; 4];
            if let Err(e) = chan_reader.read_exact(&mut len_buf).await {
                debug!("channel read ended: {e}");
                break;
            }
            let frame_len = u32::from_be_bytes(len_buf) as usize;
            if frame_len == 0 {
                continue;
            }

            let mut type_buf = [0u8; 1];
            chan_reader
                .read_exact(&mut type_buf)
                .await
                .context("failed to read message type")?;
            let msg_type = type_buf[0];

            let payload_len = frame_len - 1;

            match msg_type {
                MSG_DATA => {
                    let mut remaining = payload_len;
                    while remaining > 0 {
                        let to_read = remaining.min(read_buf.len());
                        chan_reader
                            .read_exact(&mut read_buf[..to_read])
                            .await
                            .context("failed to read data payload")?;
                        pty_write(&async_master_write, &read_buf[..to_read]).await?;
                        remaining -= to_read;
                    }
                }
                MSG_RESIZE => {
                    let mut resize_buf = [0u8; 4];
                    chan_reader
                        .read_exact(&mut resize_buf[..payload_len.min(4)])
                        .await
                        .context("failed to read resize payload")?;
                    if payload_len >= 4 {
                        let cols = u16::from_be_bytes([resize_buf[0], resize_buf[1]]);
                        let rows = u16::from_be_bytes([resize_buf[2], resize_buf[3]]);
                        debug!(cols, rows, "resizing terminal");
                        let ws = nix::pty::Winsize {
                            ws_row: rows,
                            ws_col: cols,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        };
                        unsafe {
                            libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws as *const _);
                        }
                    }
                }
                _ => {
                    let mut remaining = payload_len;
                    while remaining > 0 {
                        let to_read = remaining.min(read_buf.len());
                        chan_reader
                            .read_exact(&mut read_buf[..to_read])
                            .await
                            .context("failed to skip unknown payload")?;
                        remaining -= to_read;
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    let pty_to_channel = async {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = pty_read(&async_master_read, &mut buf).await?;
            if n == 0 {
                break;
            }
            let frame_len = (1 + n) as u32;
            chan_writer
                .write_all(&frame_len.to_be_bytes())
                .await
                .context("failed to write frame header")?;
            chan_writer
                .write_all(&[MSG_DATA])
                .await
                .context("failed to write message type")?;
            chan_writer
                .write_all(&buf[..n])
                .await
                .context("failed to write data")?;
            chan_writer.flush().await.ok();
        }
        Ok::<(), anyhow::Error>(())
    };

    let wait_child = async {
        loop {
            match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
                Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => {
                    return Ok::<_, anyhow::Error>(Some(code));
                }
                Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
                    return Ok::<_, anyhow::Error>(Some(128 + sig as i32));
                }
                Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(nix::errno::Errno::ECHILD) => {
                    return Ok::<_, anyhow::Error>(None);
                }
                _ => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    };

    tokio::select! {
        result = channel_to_pty => {
            if let Err(e) = result {
                debug!("channel to PTY ended: {e}");
            }
        }
        result = pty_to_channel => {
            if let Err(e) = result {
                debug!("PTY to channel ended: {e}");
            }
        }
        result = wait_child => {
            match result {
                Ok(Some(code)) => {
                    debug!(code, "child process exited");
                    // Send exit message to channel.
                    let frame_len = 5u32;
                    let _ = chan_writer.write_all(&frame_len.to_be_bytes()).await;
                    let _ = chan_writer.write_all(&[MSG_EXIT]).await;
                    let _ = chan_writer.write_all(&(code as i32).to_be_bytes()).await;
                    let _ = chan_writer.flush().await;
                }
                Ok(None) => {
                    debug!("child process already reaped");
                }
                Err(e) => {
                    warn!("error waiting for child: {e}");
                }
            }
        }
    }

    Ok(())
}

/// Read from a PTY master fd asynchronously.
async fn pty_read(fd: &tokio::io::unix::AsyncFd<File>, buf: &mut [u8]) -> anyhow::Result<usize> {
    Ok(fd
        .async_io(tokio::io::Interest::READABLE, |mut f| f.read(buf))
        .await?)
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

/// Clean up a child process, sending SIGKILL if it doesn't exit promptly.
async fn cleanup_child(child: Pid) {
    signal::kill(child, Signal::SIGTERM).ok();

    // Wait up to 5 seconds for the child to exit.
    for _ in 0..50 {
        match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            _ => return,
        }
    }

    warn!(?child, "child did not exit after SIGTERM, sending SIGKILL");
    signal::kill(child, Signal::SIGKILL).ok();
    waitpid(child, None).ok();
}
