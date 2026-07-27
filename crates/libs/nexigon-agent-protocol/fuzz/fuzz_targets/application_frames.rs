#![no_main]

use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use nexigon_agent_protocol::MAX_COMMAND_FRAME_LEN;
use nexigon_agent_protocol::MAX_TERMINAL_FRAME_LEN;
use nexigon_agent_protocol::read_command_frame;
use nexigon_agent_protocol::read_device_terminal_frame;
use nexigon_agent_protocol::read_hub_terminal_frame;
use tokio::io::AsyncWriteExt;

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("fuzz runtime")
});

fuzz_target!(|input: &[u8]| {
    // Keep the harness itself bounded; the codecs must independently reject peer-
    // declared lengths before allocating them.
    if input.len() > MAX_COMMAND_FRAME_LEN + 4 {
        return;
    }

    RUNTIME.block_on(async {
        for direction in 0..3 {
            let (mut tx, mut rx) = tokio::io::duplex(input.len().max(1));
            tx.write_all(input).await.unwrap();
            tx.shutdown().await.unwrap();
            match direction {
                0 if input.len() <= MAX_TERMINAL_FRAME_LEN + 4 => {
                    let _ = read_hub_terminal_frame(&mut rx).await;
                }
                1 if input.len() <= MAX_TERMINAL_FRAME_LEN + 4 => {
                    let _ = read_device_terminal_frame(&mut rx).await;
                }
                2 => {
                    let _ = read_command_frame::<serde_json::Value>(&mut rx).await;
                }
                _ => {}
            }
        }
    });
});
