use std::io;
use std::sync::Arc;
use std::sync::Mutex;

use nexigon_api::types::errors::ActionResult;
use nexigon_api::types::jwt::Jwt;
use nexigon_api::types::users::CompleteUserPasswordResetAction;
use nexigon_api::types::users::CompleteUserPasswordResetOutput;
use nexigon_rpc::execute;
use nexigon_rpc::read_action;
use nexigon_rpc::write_action_result;
use tokio::io::duplex;
use tokio::io::split;
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;

const JWT_SENTINEL: &str = "sentinel.jwt.must-not-appear";
const PASSWORD_SENTINEL: &str = "sentinel-password-must-not-appear";

/// Verify that both RPC peers log action metadata without exposing the payload.
#[tokio::test]
async fn action_payload_is_transmitted_but_never_traced() {
    let action = CompleteUserPasswordResetAction::new(
        Jwt::from_string(JWT_SENTINEL.to_owned()),
        PASSWORD_SENTINEL.to_owned(),
    );
    let output = CapturedOutput::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(Level::TRACE)
        .with_writer(output.clone())
        .finish();
    // This test has its own binary because tracing's callsite cache is process-wide.
    tracing::subscriber::set_global_default(subscriber)
        .expect("telemetry test installs the process subscriber");

    let (client, mut server) = duplex(4096);
    let (client_rx, client_tx) = split(client);
    let server_task = tokio::spawn(async move {
        let serialized = read_action(&mut server).await.expect("read action");
        let input = String::from_utf8(serialized.input().to_vec()).expect("JSON input is UTF-8");
        assert!(input.contains(JWT_SENTINEL));
        assert!(input.contains(PASSWORD_SENTINEL));
        write_action_result(
            ActionResult::Ok(CompleteUserPasswordResetOutput::Completed),
            &mut server,
        )
        .await
        .expect("write result");
    });

    let result = execute(&action, client_rx, client_tx)
        .await
        .expect("RPC transport succeeds")
        .expect("action succeeds");
    server_task.await.expect("server task succeeds");
    assert!(matches!(result, CompleteUserPasswordResetOutput::Completed));

    let output = output.contents();
    for message in ["executing action", "action has been received"] {
        assert!(
            output
                .lines()
                .any(|line| line.contains(message) && line.contains("users_CompletePasswordReset")),
            "RPC event is missing its action name: {message}"
        );
    }
    for sentinel in [JWT_SENTINEL, PASSWORD_SENTINEL] {
        assert!(!output.contains(sentinel), "telemetry leaked {sentinel:?}");
    }
}

#[derive(Clone, Default)]
struct CapturedOutput(Arc<Mutex<Vec<u8>>>);

impl CapturedOutput {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().expect("capture lock poisoned").clone())
            .expect("tracing output is UTF-8")
    }
}

struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for CapturedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("capture lock poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedOutput {
    type Writer = CapturedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CapturedWriter(self.0.clone())
    }
}
