//! Reconnection to Nexigon Hub.
//!
//! [`Reconnector`] establishes the connections of the agent's sessions. The
//! first connection is attempted right away. Before every later attempt, the
//! agent waits for a random delay between zero and an upper bound, which
//! starts at the configured initial delay and doubles after each failed
//! attempt, up to the configured maximum. A session that lasted for the
//! configured stable period resets the bound. The random delay spreads the
//! attempts of devices that lost their connections at the same time, for
//! example when Nexigon Hub restarts. If Nexigon Hub rejects an attempt with a
//! `Retry-After` header, the agent waits at least that long, up to the maximum,
//! before the random delay.

use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use jiff::Timestamp;
use nexigon_client::ClientBuilder;
use nexigon_client::ClientError;
use nexigon_client::WebsocketConnection;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing::warn;

use crate::config::Config;
use crate::hub_link::LinkController;
use crate::reconcile::random_delay;

/// Default time allowed for establishing a connection.
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;
/// Default upper bound of the delay before the first reconnection attempt.
const DEFAULT_INITIAL_DELAY_SECS: u64 = 30;
/// Default upper bound of the delay before any reconnection attempt.
const DEFAULT_MAX_DELAY_SECS: u64 = 600;
/// Default time after which a connection counts as stable.
const DEFAULT_STABLE_AFTER_SECS: u64 = 60;

/// Validated reconnection settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReconnectSettings {
    connect_timeout: Duration,
    initial_delay: Duration,
    max_delay: Duration,
    stable_after: Duration,
}

impl ReconnectSettings {
    /// Time allowed for establishing a connection.
    pub(crate) fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }
}

/// Read and validate the reconnection settings of the configuration.
pub(crate) fn reconnect_settings(config: &Config) -> anyhow::Result<ReconnectSettings> {
    let connection = config.connection.as_ref();
    let reconnect = connection.and_then(|connection| connection.reconnect.as_ref());
    let connect_timeout = connection
        .and_then(|connection| connection.connect_timeout_secs)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS);
    let initial_delay = reconnect
        .and_then(|reconnect| reconnect.initial_delay_secs)
        .unwrap_or(DEFAULT_INITIAL_DELAY_SECS);
    let max_delay = reconnect
        .and_then(|reconnect| reconnect.max_delay_secs)
        .unwrap_or(DEFAULT_MAX_DELAY_SECS);
    let stable_after = reconnect
        .and_then(|reconnect| reconnect.stable_after_secs)
        .unwrap_or(DEFAULT_STABLE_AFTER_SECS);
    if connect_timeout == 0 {
        anyhow::bail!("connection connect-timeout-secs must be at least 1");
    }
    if initial_delay == 0 {
        anyhow::bail!("connection reconnect initial-delay-secs must be at least 1");
    }
    if max_delay < initial_delay {
        anyhow::bail!(
            "connection reconnect max-delay-secs must not be lower than initial-delay-secs"
        );
    }
    Ok(ReconnectSettings {
        connect_timeout: Duration::from_secs(connect_timeout),
        initial_delay: Duration::from_secs(initial_delay),
        max_delay: Duration::from_secs(max_delay),
        stable_after: Duration::from_secs(stable_after),
    })
}

/// Establishes the connections of the agent's sessions.
pub(crate) struct Reconnector {
    client: ClientBuilder,
    settings: ReconnectSettings,
    backoff: Backoff,
    /// Whether a connection has been attempted yet.
    attempted: bool,
    /// Time at which the last session started, if it has not been
    /// accounted for yet.
    session_started_at: Option<Instant>,
}

impl Reconnector {
    pub(crate) fn new(client: ClientBuilder, settings: ReconnectSettings) -> Self {
        Self {
            client,
            settings,
            backoff: Backoff::new(settings.initial_delay, settings.max_delay),
            attempted: false,
            session_started_at: None,
        }
    }

    /// Record that a session has started on the last connection, which counts
    /// toward the stable period.
    pub(crate) fn session_started(&mut self) {
        self.session_started_at = Some(Instant::now());
    }

    /// Establish the next connection, or return `None` once `shutdown` is
    /// cancelled.
    ///
    /// Fails if the configuration prevents any connection, for example because
    /// it disallows plaintext connections to a plaintext Hub URL.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn connect(
        &mut self,
        link: &LinkController,
        shutdown: &CancellationToken,
    ) -> anyhow::Result<Option<WebsocketConnection>> {
        if shutdown.is_cancelled() {
            return Ok(None);
        }
        if let Some(started_at) = self.session_started_at.take()
            && started_at.elapsed() >= self.settings.stable_after
        {
            self.backoff.reset();
        }
        let mut delay = if self.attempted {
            self.backoff.next_delay()
        } else {
            Duration::ZERO
        };
        loop {
            if !delay.is_zero() {
                info!(?delay, "waiting before connecting to Nexigon Hub");
                link.next_attempt_at(Timestamp::now().checked_add(delay).ok());
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return Ok(None),
                    () = tokio::time::sleep(delay) => {}
                }
                link.next_attempt_at(None);
            }
            self.attempted = true;
            let attempt =
                tokio::time::timeout(self.settings.connect_timeout, self.client.connect());
            let result = tokio::select! {
                biased;
                () = shutdown.cancelled() => return Ok(None),
                result = attempt => result,
            };
            let error = match result {
                Ok(Ok(connection)) => return Ok(Some(connection)),
                Ok(Err(error)) if is_permanent(&error) => {
                    return Err(error).context("cannot connect to Nexigon Hub");
                }
                Ok(Err(error)) => error,
                Err(_) => {
                    warn!("connecting to Nexigon Hub timed out");
                    link.attempt_failed("connecting to Nexigon Hub timed out".to_owned());
                    delay = self.backoff.next_delay();
                    continue;
                }
            };
            warn!(
                error = format!("{error:#}"),
                "failed to connect to Nexigon Hub"
            );
            link.attempt_failed(format!("failed to connect to Nexigon Hub: {error}"));
            let retry_after = error.retry_after().map_or(Duration::ZERO, |retry_after| {
                retry_after.min(self.settings.max_delay)
            });
            delay = retry_after + self.backoff.next_delay();
        }
    }
}

/// Exponential backoff with full jitter.
#[derive(Debug)]
struct Backoff {
    initial: Duration,
    max: Duration,
    /// Delays handed out since the last reset.
    attempts: u32,
}

impl Backoff {
    fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            attempts: 0,
        }
    }

    /// Random delay of up to the current bound, which then doubles.
    fn next_delay(&mut self) -> Duration {
        random_delay(self.next_bound())
    }

    fn next_bound(&mut self) -> Duration {
        let factor = 1u32.checked_shl(self.attempts).unwrap_or(u32::MAX);
        self.attempts = self.attempts.saturating_add(1);
        self.initial.saturating_mul(factor).min(self.max)
    }

    fn reset(&mut self) {
        self.attempts = 0;
    }
}

/// Whether retrying cannot help because the error stems from the configuration.
fn is_permanent(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::UnsupportedUrlScheme(_)
            | ClientError::PlaintextTransportDisabled
            | ClientError::InvalidClientIdentity { .. }
            | ClientError::CryptoProviderUnavailable
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Backoff;

    /// The bound doubles up to the maximum without overflowing, and a reset
    /// starts over at the initial bound.
    #[test]
    fn backoff_bound_doubles_up_to_the_maximum() {
        let mut backoff = Backoff::new(Duration::from_secs(30), Duration::from_secs(100));
        let bounds: Vec<_> = (0..40).map(|_| backoff.next_bound().as_secs()).collect();
        assert_eq!(bounds[..4], [30, 60, 100, 100]);
        assert!(bounds.iter().all(|bound| *bound <= 100));
        backoff.reset();
        assert_eq!(backoff.next_bound(), Duration::from_secs(30));
    }
}
