//! Integration tests for the multiplex protocol.
//!
//! These tests use the `InMemory` transport to verify the protocol behavior
//! end-to-end without any real network I/O.

use std::sync::Once;

use bytes::Bytes;
use futures::StreamExt;
use nexigon_multiplex::Channel;
use nexigon_multiplex::Connection;
use nexigon_multiplex::ConnectionEvent;
use nexigon_multiplex::transport::InMemory;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

static INIT_TRACING: Once = Once::new();

fn init_tracing() {
    INIT_TRACING.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("trace")),
            )
            .with_test_writer()
            .init();
    });
}

/// Helper: create a connected pair of connections with background drivers.
///
/// Returns:
/// - `conn_a_ref`: ConnectionRef for side A (can open channels)
/// - `chan_rx_a`: receives channels accepted by side A's driver
/// - `conn_b_ref`: ConnectionRef for side B (can open channels)
/// - `chan_rx_b`: receives channels accepted by side B's driver
/// - Two JoinHandles for the background drivers
struct TestPair {
    conn_a_ref: nexigon_multiplex::ConnectionRef,
    chan_rx_a: tokio::sync::mpsc::UnboundedReceiver<(String, Channel)>,
    conn_b_ref: nexigon_multiplex::ConnectionRef,
    chan_rx_b: tokio::sync::mpsc::UnboundedReceiver<(String, Channel)>,
    _driver_a: tokio::task::JoinHandle<()>,
    _driver_b: tokio::task::JoinHandle<()>,
}

impl TestPair {
    fn new() -> Self {
        Self::with_buffer(32)
    }

    fn with_buffer(buffer: usize) -> Self {
        let (transport_a, transport_b) = InMemory::<Bytes, Bytes>::new_buffered(buffer);

        let mut conn_a = Connection::new(transport_a);
        let mut conn_b = Connection::new(transport_b);

        let conn_a_ref = conn_a.make_ref();
        let conn_b_ref = conn_b.make_ref();

        let (chan_tx_a, chan_rx_a) = tokio::sync::mpsc::unbounded_channel();
        let (chan_tx_b, chan_rx_b) = tokio::sync::mpsc::unbounded_channel();

        let driver_a = tokio::spawn(async move {
            while let Some(event) = conn_a.next().await {
                match event {
                    Ok(ConnectionEvent::Connected) => {
                        tracing::debug!("side A: connected");
                    }
                    Ok(ConnectionEvent::Closed) => {
                        tracing::debug!("side A: closed");
                        break;
                    }
                    Ok(ConnectionEvent::RequestChannel(request)) => {
                        let endpoint = String::from_utf8_lossy(request.endpoint()).to_string();
                        tracing::debug!(endpoint, "side A: accepting channel");
                        let chan_tx = chan_tx_a.clone();
                        request.accept(move |channel| {
                            chan_tx.send((endpoint, channel)).ok();
                        });
                    }
                    Err(e) => {
                        tracing::error!("side A: connection error: {e}");
                        break;
                    }
                }
            }
            tracing::debug!("side A: driver exiting");
        });

        let driver_b = tokio::spawn(async move {
            while let Some(event) = conn_b.next().await {
                match event {
                    Ok(ConnectionEvent::Connected) => {
                        tracing::debug!("side B: connected");
                    }
                    Ok(ConnectionEvent::Closed) => {
                        tracing::debug!("side B: closed");
                        break;
                    }
                    Ok(ConnectionEvent::RequestChannel(request)) => {
                        let endpoint = String::from_utf8_lossy(request.endpoint()).to_string();
                        tracing::debug!(endpoint, "side B: accepting channel");
                        let chan_tx = chan_tx_b.clone();
                        request.accept(move |channel| {
                            chan_tx.send((endpoint, channel)).ok();
                        });
                    }
                    Err(e) => {
                        tracing::error!("side B: connection error: {e}");
                        break;
                    }
                }
            }
            tracing::debug!("side B: driver exiting");
        });

        Self {
            conn_a_ref,
            chan_rx_a,
            conn_b_ref,
            chan_rx_b,
            _driver_a: driver_a,
            _driver_b: driver_b,
        }
    }
}

#[tokio::test]
async fn test_connection_handshake() {
    init_tracing();

    let (transport_a, transport_b) = InMemory::<Bytes, Bytes>::new_buffered(32);
    let mut conn_a = Connection::new(transport_a);
    let mut conn_b = Connection::new(transport_b);

    // Both sides must be polled concurrently since InMemory transport
    // requires both ends to be active for frames to flow.
    let (event_a, event_b) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        futures::future::join(conn_a.next(), conn_b.next()),
    )
    .await
    .expect("timeout waiting for handshake");

    let event_a = event_a.expect("conn_a stream ended");
    assert!(matches!(event_a, Ok(ConnectionEvent::Connected)));

    let event_b = event_b.expect("conn_b stream ended");
    assert!(matches!(event_b, Ok(ConnectionEvent::Connected)));
}

#[tokio::test]
async fn test_open_channel() {
    init_tracing();

    let mut pair = TestPair::new();

    // Side A opens a channel to side B.
    let channel_a = pair
        .conn_a_ref
        .open(b"test-endpoint")
        .await
        .expect("failed to open channel");

    // Side B should receive the accepted channel.
    let (endpoint, _channel_b) =
        tokio::time::timeout(std::time::Duration::from_secs(5), pair.chan_rx_b.recv())
            .await
            .expect("timeout")
            .expect("channel not received");

    assert_eq!(endpoint, "test-endpoint");

    drop(channel_a);
}

#[tokio::test]
async fn test_channel_data_a_to_b() {
    init_tracing();

    let mut pair = TestPair::new();

    // A opens channel to B.
    let mut channel_a = pair
        .conn_a_ref
        .open(b"data-test")
        .await
        .expect("failed to open channel");

    let (_, mut channel_b) =
        tokio::time::timeout(std::time::Duration::from_secs(5), pair.chan_rx_b.recv())
            .await
            .expect("timeout")
            .expect("channel not received");

    // Write data from A to B.
    let test_data = b"Hello from A to B!";
    channel_a.write_all(test_data).await.expect("write failed");
    channel_a.flush().await.expect("flush failed");

    // Read data on B.
    let mut buf = vec![0u8; test_data.len()];
    channel_b.read_exact(&mut buf).await.expect("read failed");

    assert_eq!(&buf, test_data);
}

#[tokio::test]
async fn test_channel_data_b_to_a() {
    init_tracing();

    let mut pair = TestPair::new();

    // A opens channel to B.
    let mut channel_a = pair
        .conn_a_ref
        .open(b"reverse-data-test")
        .await
        .expect("failed to open channel");

    let (_, mut channel_b) =
        tokio::time::timeout(std::time::Duration::from_secs(5), pair.chan_rx_b.recv())
            .await
            .expect("timeout")
            .expect("channel not received");

    // Write data from B to A.
    let test_data = b"Hello from B to A!";
    channel_b.write_all(test_data).await.expect("write failed");
    channel_b.flush().await.expect("flush failed");

    // Read data on A.
    let mut buf = vec![0u8; test_data.len()];
    channel_a.read_exact(&mut buf).await.expect("read failed");

    assert_eq!(&buf, test_data);
}

#[tokio::test]
async fn test_bidirectional_data() {
    init_tracing();

    let mut pair = TestPair::new();

    let channel_a = pair
        .conn_a_ref
        .open(b"bidir-test")
        .await
        .expect("failed to open channel");

    let (_, channel_b) =
        tokio::time::timeout(std::time::Duration::from_secs(5), pair.chan_rx_b.recv())
            .await
            .expect("timeout")
            .expect("channel not received");

    let (mut a_writer, mut a_reader) = channel_a.split();
    let (mut b_writer, mut b_reader) = channel_b.split();

    let msg_a = b"Message from A";
    let msg_b = b"Message from B";

    // Send simultaneously in both directions.
    let write_a = async {
        a_writer.write_all(msg_a).await.expect("A write failed");
        a_writer.flush().await.expect("A flush failed");
    };

    let write_b = async {
        b_writer.write_all(msg_b).await.expect("B write failed");
        b_writer.flush().await.expect("B flush failed");
    };

    let read_a = async {
        let mut buf = vec![0u8; msg_b.len()];
        a_reader.read_exact(&mut buf).await.expect("A read failed");
        assert_eq!(&buf, msg_b);
    };

    let read_b = async {
        let mut buf = vec![0u8; msg_a.len()];
        b_reader.read_exact(&mut buf).await.expect("B read failed");
        assert_eq!(&buf, msg_a);
    };

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        futures::future::join4(write_a, write_b, read_a, read_b),
    )
    .await
    .expect("timeout in bidirectional test");
}

#[tokio::test]
async fn test_large_data_transfer() {
    init_tracing();

    let mut pair = TestPair::new();

    let channel_a = pair
        .conn_a_ref
        .open(b"large-data")
        .await
        .expect("failed to open channel");

    let (_, channel_b) =
        tokio::time::timeout(std::time::Duration::from_secs(5), pair.chan_rx_b.recv())
            .await
            .expect("timeout")
            .expect("channel not received");

    let (mut a_writer, mut a_reader) = channel_a.split();
    let (mut b_writer, mut b_reader) = channel_b.split();

    // Send 256 KiB of data - enough to exhaust initial 16 KiB byte credit
    // and trigger credit replenishment via ChannelAdjust.
    let data_size = 256 * 1024;
    let send_data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();

    let send_data_clone = send_data.clone();
    let writer = tokio::spawn(async move {
        a_writer
            .write_all(&send_data_clone)
            .await
            .expect("write failed");
        a_writer.flush().await.expect("flush failed");
        tracing::debug!("large write complete");
    });

    let reader = tokio::spawn(async move {
        let mut received = vec![0u8; data_size];
        b_reader
            .read_exact(&mut received)
            .await
            .expect("read failed");
        tracing::debug!("large read complete");
        received
    });

    let (_, received) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        futures::future::join(writer, reader),
    )
    .await
    .expect("timeout in large data transfer");

    let received = received.expect("reader task panicked");
    assert_eq!(received.len(), data_size);
    assert_eq!(received, send_data);

    // Also test the reverse direction.
    let reverse_data: Vec<u8> = (0..data_size).map(|i| (255 - (i % 256)) as u8).collect();
    let reverse_data_clone = reverse_data.clone();

    let writer = tokio::spawn(async move {
        b_writer
            .write_all(&reverse_data_clone)
            .await
            .expect("reverse write failed");
        b_writer.flush().await.expect("reverse flush failed");
    });

    let reader = tokio::spawn(async move {
        let mut received = vec![0u8; data_size];
        a_reader
            .read_exact(&mut received)
            .await
            .expect("reverse read failed");
        received
    });

    let (_, received) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        futures::future::join(writer, reader),
    )
    .await
    .expect("timeout in reverse large data transfer");

    let received = received.expect("reverse reader task panicked");
    assert_eq!(received, reverse_data);
}

#[tokio::test]
async fn test_channel_close_sender() {
    init_tracing();

    let mut pair = TestPair::new();

    let channel_a = pair
        .conn_a_ref
        .open(b"close-test")
        .await
        .expect("failed to open channel");

    let (_, mut channel_b) =
        tokio::time::timeout(std::time::Duration::from_secs(5), pair.chan_rx_b.recv())
            .await
            .expect("timeout")
            .expect("channel not received");

    let (mut a_writer, _a_reader) = channel_a.split();

    // Write some data then explicitly shutdown (sends ChannelClosed frame).
    // Note: simply dropping the Sender does NOT send a close frame.
    a_writer
        .write_all(b"before close")
        .await
        .expect("write failed");
    a_writer.flush().await.expect("flush failed");
    a_writer.shutdown().await.expect("shutdown failed");

    // B should be able to read the data, then get EOF.
    let mut buf = vec![0u8; 12];
    channel_b.read_exact(&mut buf).await.expect("read failed");
    assert_eq!(&buf, b"before close");

    // Next read should return 0 (EOF) since the sender was dropped.
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), channel_b.read(&mut buf))
        .await
        .expect("timeout waiting for EOF")
        .expect("read error");
    assert_eq!(n, 0, "expected EOF after sender dropped");
}

#[tokio::test]
async fn test_multiple_channels() {
    init_tracing();

    let mut pair = TestPair::new();

    // Open three channels from A to B.
    let mut channel_a1 = pair
        .conn_a_ref
        .open(b"chan-1")
        .await
        .expect("failed to open channel 1");
    let (ep1, mut channel_b1) = pair.chan_rx_b.recv().await.expect("no channel 1");
    assert_eq!(ep1, "chan-1");

    let mut channel_a2 = pair
        .conn_a_ref
        .open(b"chan-2")
        .await
        .expect("failed to open channel 2");
    let (ep2, mut channel_b2) = pair.chan_rx_b.recv().await.expect("no channel 2");
    assert_eq!(ep2, "chan-2");

    let mut channel_a3 = pair
        .conn_a_ref
        .open(b"chan-3")
        .await
        .expect("failed to open channel 3");
    let (ep3, mut channel_b3) = pair.chan_rx_b.recv().await.expect("no channel 3");
    assert_eq!(ep3, "chan-3");

    // Send different data on each channel.
    channel_a1
        .write_all(b"data-for-ch-1")
        .await
        .expect("write 1 failed");
    channel_a1.flush().await.expect("flush 1 failed");

    channel_a2
        .write_all(b"data-for-ch-2")
        .await
        .expect("write 2 failed");
    channel_a2.flush().await.expect("flush 2 failed");

    channel_a3
        .write_all(b"data-for-ch-3")
        .await
        .expect("write 3 failed");
    channel_a3.flush().await.expect("flush 3 failed");

    // Read on each channel independently.
    let mut buf1 = vec![0u8; 13];
    channel_b1
        .read_exact(&mut buf1)
        .await
        .expect("read 1 failed");
    assert_eq!(&buf1, b"data-for-ch-1");

    let mut buf2 = vec![0u8; 13];
    channel_b2
        .read_exact(&mut buf2)
        .await
        .expect("read 2 failed");
    assert_eq!(&buf2, b"data-for-ch-2");

    let mut buf3 = vec![0u8; 13];
    channel_b3
        .read_exact(&mut buf3)
        .await
        .expect("read 3 failed");
    assert_eq!(&buf3, b"data-for-ch-3");
}

#[tokio::test]
async fn test_channel_split_and_merge() {
    init_tracing();

    let mut pair = TestPair::new();

    let channel_a = pair
        .conn_a_ref
        .open(b"split-test")
        .await
        .expect("failed to open channel");

    let (_, channel_b) = pair.chan_rx_b.recv().await.expect("no channel");

    // Split channel A.
    let (mut a_sender, mut a_receiver) = channel_a.split();

    // Split channel B.
    let (mut b_sender, mut b_receiver) = channel_b.split();

    // Use split halves for bidirectional communication.
    a_sender.write_all(b"from-a").await.expect("a write");
    a_sender.flush().await.expect("a flush");

    b_sender.write_all(b"from-b").await.expect("b write");
    b_sender.flush().await.expect("b flush");

    let mut buf = vec![0u8; 6];
    b_receiver.read_exact(&mut buf).await.expect("b read");
    assert_eq!(&buf, b"from-a");

    a_receiver.read_exact(&mut buf).await.expect("a read");
    assert_eq!(&buf, b"from-b");

    // Merge back and verify it still works.
    let mut merged = Channel::merge(a_sender, a_receiver);
    merged.write_all(b"merged").await.expect("merged write");
    merged.flush().await.expect("merged flush");

    let mut buf = vec![0u8; 6];
    b_receiver.read_exact(&mut buf).await.expect("merged read");
    assert_eq!(&buf, b"merged");
}

#[tokio::test]
async fn test_multiple_writes_before_flush() {
    init_tracing();

    let mut pair = TestPair::new();

    let channel_a = pair
        .conn_a_ref
        .open(b"multi-write")
        .await
        .expect("failed to open channel");

    let (_, mut channel_b) = pair.chan_rx_b.recv().await.expect("no channel");

    let (mut a_writer, _a_reader) = channel_a.split();

    let payload = b"terminal data payload here!";
    let frame_len = (1 + payload.len()) as u32;
    a_writer
        .write_all(&frame_len.to_be_bytes())
        .await
        .expect("write len");
    a_writer.write_all(&[0x00]).await.expect("write type");
    a_writer.write_all(payload).await.expect("write payload");
    a_writer.flush().await.expect("flush");

    let total_len = 4 + 1 + payload.len();
    let mut buf = vec![0u8; total_len];
    channel_b.read_exact(&mut buf).await.expect("read failed");
    assert_eq!(&buf[0..4], &frame_len.to_be_bytes());
    assert_eq!(buf[4], 0x00);
    assert_eq!(&buf[5..], payload);
}

#[tokio::test]
async fn test_channel_open_from_both_sides() {
    init_tracing();

    let mut pair = TestPair::new();

    // A opens a channel to B.
    let mut channel_a = pair
        .conn_a_ref
        .open(b"from-a")
        .await
        .expect("A open failed");

    let (ep, mut channel_b) = pair.chan_rx_b.recv().await.expect("B didn't get channel");
    assert_eq!(ep, "from-a");

    // B opens a channel to A.
    let mut channel_b2 = pair
        .conn_b_ref
        .open(b"from-b")
        .await
        .expect("B open failed");

    let (ep, mut channel_a2) = pair.chan_rx_a.recv().await.expect("A didn't get channel");
    assert_eq!(ep, "from-b");

    // Data flows independently on both channels.
    channel_a.write_all(b"a->b").await.unwrap();
    channel_a.flush().await.unwrap();
    channel_b2.write_all(b"b->a").await.unwrap();
    channel_b2.flush().await.unwrap();

    let mut buf = vec![0u8; 4];
    channel_b.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"a->b");

    channel_a2.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"b->a");
}
