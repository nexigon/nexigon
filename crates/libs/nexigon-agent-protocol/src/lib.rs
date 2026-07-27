//! Shared framing for agent/hub application protocols.
//!
//! These codecs deliberately retain the legacy wire representation while enforcing
//! resource limits before allocating a peer-declared payload. A peer that violates a
//! limit or sends a malformed frame has left the protocol; callers must close the
//! multiplex channel instead of trying to recover stream alignment.

use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

/// Maximum terminal data carried by one application frame.
pub const MAX_TERMINAL_DATA_LEN: usize = 1024 * 1024;
/// Maximum terminal frame body, including the one-byte message type.
pub const MAX_TERMINAL_FRAME_LEN: usize = MAX_TERMINAL_DATA_LEN + 1;
/// Maximum JSON payload carried by one command frame.
pub const MAX_COMMAND_FRAME_LEN: usize = 16 * 1024 * 1024;
/// Deadline for the rest of a frame after its first header byte arrives.
pub const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum number of external commands the agent runs concurrently.
pub const MAX_CONCURRENT_COMMANDS: usize = 4;
/// Maximum absolute external-command runtime, including streamed commands.
pub const MAX_COMMAND_RUNTIME: Duration = Duration::from_secs(60 * 60);
/// Maximum bytes read from a command's stdout and stderr combined.
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
/// Maximum stdout/stderr lines read from one command.
pub const MAX_COMMAND_OUTPUT_LINES: usize = 16 * 1024;
/// Maximum bytes in one stdout/stderr line.
pub const MAX_COMMAND_OUTPUT_LINE_LEN: usize = 64 * 1024;

const TERMINAL_DATA: u8 = 0x00;
const TERMINAL_RESIZE: u8 = 0x01;
const TERMINAL_EXIT: u8 = 0x02;

/// A terminal frame sent from the hub to an agent.
#[derive(Debug, Eq, PartialEq)]
pub enum HubTerminalFrame {
    /// Bytes to write to the terminal.
    Data(Vec<u8>),
    /// Updated terminal dimensions.
    Resize { cols: u16, rows: u16 },
}

/// A terminal frame sent from an agent to the hub.
#[derive(Debug, Eq, PartialEq)]
pub enum DeviceTerminalFrame {
    /// Bytes read from the terminal.
    Data(Vec<u8>),
    /// Final child process status.
    Exit(i32),
}

/// A malformed, oversized, truncated, or stalled application frame.
#[derive(Debug, Error)]
pub enum FrameError {
    /// Underlying channel I/O failed.
    #[error("failed to read or write application frame: {0}")]
    Io(#[from] std::io::Error),
    /// A declared frame body is empty.
    #[error("application frame length must not be zero")]
    Empty,
    /// A peer-declared frame body exceeds its protocol limit.
    #[error("application frame length {actual} exceeds limit {limit}")]
    TooLarge { actual: usize, limit: usize },
    /// A fixed-size frame has the wrong body length.
    #[error("terminal frame type {kind:#04x} has length {actual}; expected {expected}")]
    InvalidTerminalLength {
        kind: u8,
        actual: usize,
        expected: usize,
    },
    /// The message type is unknown or invalid in this direction.
    #[error("terminal frame type {0:#04x} is not valid in this direction")]
    InvalidTerminalType(u8),
    /// The peer stopped making progress partway through a frame.
    #[error("timed out while reading an application frame")]
    Timeout,
    /// JSON serialization or deserialization failed.
    #[error("invalid command frame JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Read and validate one hub-to-agent terminal frame.
pub async fn read_hub_terminal_frame(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<HubTerminalFrame, FrameError> {
    let (frame_len, kind) = read_terminal_header(reader).await?;
    match kind {
        TERMINAL_DATA => Ok(HubTerminalFrame::Data(
            read_payload(reader, frame_len - 1).await?,
        )),
        TERMINAL_RESIZE => {
            require_terminal_len(kind, frame_len, 5)?;
            let payload = read_array::<4>(reader).await?;
            Ok(HubTerminalFrame::Resize {
                cols: u16::from_be_bytes([payload[0], payload[1]]),
                rows: u16::from_be_bytes([payload[2], payload[3]]),
            })
        }
        _ => Err(FrameError::InvalidTerminalType(kind)),
    }
}

/// Read and validate one agent-to-hub terminal frame.
pub async fn read_device_terminal_frame(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<DeviceTerminalFrame, FrameError> {
    let (frame_len, kind) = read_terminal_header(reader).await?;
    match kind {
        TERMINAL_DATA => Ok(DeviceTerminalFrame::Data(
            read_payload(reader, frame_len - 1).await?,
        )),
        TERMINAL_EXIT => {
            require_terminal_len(kind, frame_len, 5)?;
            Ok(DeviceTerminalFrame::Exit(i32::from_be_bytes(
                read_array::<4>(reader).await?,
            )))
        }
        _ => Err(FrameError::InvalidTerminalType(kind)),
    }
}

/// Write one terminal data frame in either direction.
pub async fn write_terminal_data(
    writer: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
) -> Result<(), FrameError> {
    if data.len() > MAX_TERMINAL_DATA_LEN {
        return Err(FrameError::TooLarge {
            actual: data.len(),
            limit: MAX_TERMINAL_DATA_LEN,
        });
    }
    write_parts(writer, TERMINAL_DATA, data).await
}

/// Write one hub-to-agent resize frame.
pub async fn write_terminal_resize(
    writer: &mut (impl AsyncWrite + Unpin),
    cols: u16,
    rows: u16,
) -> Result<(), FrameError> {
    let mut payload = [0; 4];
    payload[..2].copy_from_slice(&cols.to_be_bytes());
    payload[2..].copy_from_slice(&rows.to_be_bytes());
    write_parts(writer, TERMINAL_RESIZE, &payload).await
}

/// Write one agent-to-hub exit frame.
pub async fn write_terminal_exit(
    writer: &mut (impl AsyncWrite + Unpin),
    code: i32,
) -> Result<(), FrameError> {
    write_parts(writer, TERMINAL_EXIT, &code.to_be_bytes()).await
}

/// Read one bounded length-prefixed JSON command frame.
pub async fn read_command_frame<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<T, FrameError> {
    let len = read_length(reader, MAX_COMMAND_FRAME_LEN).await?;
    let data = read_payload(reader, len).await?;
    Ok(serde_json::from_slice(&data)?)
}

/// Write one bounded length-prefixed JSON command frame.
pub async fn write_command_frame<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &T,
) -> Result<(), FrameError> {
    let data = serde_json::to_vec(frame)?;
    if data.len() > MAX_COMMAND_FRAME_LEN {
        return Err(FrameError::TooLarge {
            actual: data.len(),
            limit: MAX_COMMAND_FRAME_LEN,
        });
    }
    let len = u32::try_from(data.len()).expect("command frame limit fits in u32");
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&data).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_terminal_header(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<(usize, u8), FrameError> {
    let frame_len = read_length(reader, MAX_TERMINAL_FRAME_LEN).await?;
    let kind = read_array::<1>(reader).await?[0];
    Ok((frame_len, kind))
}

fn require_terminal_len(kind: u8, actual: usize, expected: usize) -> Result<(), FrameError> {
    if actual != expected {
        return Err(FrameError::InvalidTerminalLength {
            kind,
            actual,
            expected,
        });
    }
    Ok(())
}

async fn read_length(
    reader: &mut (impl AsyncRead + Unpin),
    limit: usize,
) -> Result<usize, FrameError> {
    // Waiting for the first byte is allowed so an idle interactive terminal remains
    // idle. Once a frame begins, every remaining read shares a strict deadline.
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes[..1]).await?;
    read_exact_with_timeout(reader, &mut bytes[1..]).await?;
    let len = u32::from_be_bytes(bytes) as usize;
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > limit {
        return Err(FrameError::TooLarge { actual: len, limit });
    }
    Ok(len)
}

async fn read_payload(
    reader: &mut (impl AsyncRead + Unpin),
    len: usize,
) -> Result<Vec<u8>, FrameError> {
    let mut data = vec![0; len];
    read_exact_with_timeout(reader, &mut data).await?;
    Ok(data)
}

async fn read_array<const N: usize>(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<[u8; N], FrameError> {
    let mut data = [0; N];
    read_exact_with_timeout(reader, &mut data).await?;
    Ok(data)
}

async fn read_exact_with_timeout(
    reader: &mut (impl AsyncRead + Unpin),
    data: &mut [u8],
) -> Result<(), FrameError> {
    tokio::time::timeout(FRAME_READ_TIMEOUT, reader.read_exact(data))
        .await
        .map_err(|_| FrameError::Timeout)??;
    Ok(())
}

async fn write_parts(
    writer: &mut (impl AsyncWrite + Unpin),
    kind: u8,
    payload: &[u8],
) -> Result<(), FrameError> {
    let frame_len = u32::try_from(payload.len() + 1).expect("terminal frame limit fits in u32");
    writer.write_all(&frame_len.to_be_bytes()).await?;
    writer.write_all(&[kind]).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde::Deserialize;
    use serde::Serialize;
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct JsonFrame {
        value: String,
    }

    async fn terminal_input(len: u32, body: &[u8]) -> tokio::io::DuplexStream {
        let (mut tx, rx) = tokio::io::duplex(body.len() + 4);
        tx.write_all(&len.to_be_bytes()).await.unwrap();
        tx.write_all(body).await.unwrap();
        tx.shutdown().await.unwrap();
        rx
    }

    #[tokio::test]
    async fn terminal_boundaries_are_checked_before_payload_allocation() {
        let mut empty = terminal_input(0, &[]).await;
        assert!(matches!(
            read_hub_terminal_frame(&mut empty).await,
            Err(FrameError::Empty)
        ));

        let exact_body = [vec![TERMINAL_DATA], vec![0; MAX_TERMINAL_DATA_LEN]].concat();
        let mut exact = terminal_input(MAX_TERMINAL_FRAME_LEN as u32, &exact_body).await;
        assert_eq!(
            read_hub_terminal_frame(&mut exact).await.unwrap(),
            HubTerminalFrame::Data(vec![0; MAX_TERMINAL_DATA_LEN])
        );

        for len in [MAX_TERMINAL_FRAME_LEN as u32 + 1, u32::MAX] {
            let mut oversized = terminal_input(len, &[]).await;
            assert!(matches!(
                read_hub_terminal_frame(&mut oversized).await,
                Err(FrameError::TooLarge { actual, .. }) if actual == len as usize
            ));
        }
    }

    #[tokio::test]
    async fn truncated_headers_and_bodies_are_errors() {
        for header in [vec![], vec![0], vec![0, 0], vec![0, 0, 0]] {
            let (mut tx, mut rx) = tokio::io::duplex(8);
            tx.write_all(&header).await.unwrap();
            tx.shutdown().await.unwrap();
            assert!(matches!(
                read_hub_terminal_frame(&mut rx).await,
                Err(FrameError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
            ));
        }

        let mut body = terminal_input(4, &[TERMINAL_DATA, 1, 2]).await;
        assert!(matches!(
            read_hub_terminal_frame(&mut body).await,
            Err(FrameError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
    }

    #[tokio::test]
    async fn direction_types_and_fixed_lengths_are_strict() {
        let mut unknown = terminal_input(1, &[0xff]).await;
        assert!(matches!(
            read_hub_terminal_frame(&mut unknown).await,
            Err(FrameError::InvalidTerminalType(0xff))
        ));

        let mut oversized_resize = terminal_input(6, &[TERMINAL_RESIZE, 0, 1, 0, 2, 9]).await;
        assert!(matches!(
            read_hub_terminal_frame(&mut oversized_resize).await,
            Err(FrameError::InvalidTerminalLength {
                kind: TERMINAL_RESIZE,
                actual: 6,
                expected: 5,
            })
        ));

        let mut exit_from_hub = terminal_input(5, &[TERMINAL_EXIT, 0, 0, 0, 7]).await;
        assert!(matches!(
            read_hub_terminal_frame(&mut exit_from_hub).await,
            Err(FrameError::InvalidTerminalType(TERMINAL_EXIT))
        ));

        let mut resize_from_device = terminal_input(5, &[TERMINAL_RESIZE, 0, 80, 0, 24]).await;
        assert!(matches!(
            read_device_terminal_frame(&mut resize_from_device).await,
            Err(FrameError::InvalidTerminalType(TERMINAL_RESIZE))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_stalls_mid_frame_hits_the_deadline() {
        let (mut tx, mut rx) = tokio::io::duplex(8);
        tx.write_all(&[0]).await.unwrap();
        let read = tokio::spawn(async move { read_hub_terminal_frame(&mut rx).await });
        tokio::task::yield_now().await;
        tokio::time::advance(FRAME_READ_TIMEOUT + Duration::from_millis(1)).await;
        assert!(matches!(read.await.unwrap(), Err(FrameError::Timeout)));

        let (mut tx, mut rx) = tokio::io::duplex(8);
        tx.write_all(&2u32.to_be_bytes()).await.unwrap();
        tx.write_all(&[TERMINAL_DATA]).await.unwrap();
        let read = tokio::spawn(async move { read_hub_terminal_frame(&mut rx).await });
        tokio::task::yield_now().await;
        tokio::time::advance(FRAME_READ_TIMEOUT + Duration::from_millis(1)).await;
        assert!(matches!(read.await.unwrap(), Err(FrameError::Timeout)));
    }

    #[tokio::test]
    async fn command_frames_reject_zero_limit_plus_one_and_max_u32() {
        let mut exact = terminal_input(MAX_COMMAND_FRAME_LEN as u32, &[]).await;
        assert!(matches!(
            read_command_frame::<JsonFrame>(&mut exact).await,
            Err(FrameError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));

        for len in [0, MAX_COMMAND_FRAME_LEN as u32 + 1, u32::MAX] {
            let mut input = terminal_input(len, &[]).await;
            let result = read_command_frame::<JsonFrame>(&mut input).await;
            assert!(matches!(
                result,
                Err(FrameError::Empty | FrameError::TooLarge { .. })
            ));
        }
    }

    #[tokio::test]
    async fn command_frame_round_trip_and_truncation() {
        let expected = JsonFrame {
            value: "hello".to_owned(),
        };
        let (mut tx, mut rx) = tokio::io::duplex(256);
        write_command_frame(&mut tx, &expected).await.unwrap();
        assert_eq!(
            read_command_frame::<JsonFrame>(&mut rx).await.unwrap(),
            expected
        );

        let mut truncated = terminal_input(8, br#"{"v"#).await;
        assert!(matches!(
            read_command_frame::<JsonFrame>(&mut truncated).await,
            Err(FrameError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
    }

    proptest! {
        #[test]
        fn any_oversized_declared_terminal_length_is_rejected(
            extra in 1u32..=(u32::MAX - MAX_TERMINAL_FRAME_LEN as u32)
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let len = MAX_TERMINAL_FRAME_LEN as u32 + extra;
                let mut input = terminal_input(len, &[]).await;
                let rejected = matches!(
                    read_device_terminal_frame(&mut input).await,
                    Err(FrameError::TooLarge { actual, .. }) if actual == len as usize
                );
                prop_assert!(rejected);
                Ok(())
            })?;
        }
    }
}
