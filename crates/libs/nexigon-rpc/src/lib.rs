//! Simple RPC protocol for executing actions over arbitrary transports.
//!
//! The wire format is intentionally small and unchanged:
//!
//! - request: `u16 name length`, UTF-8 name, `u32 input length`, JSON input;
//! - response: `u32 output length`, JSON [`ActionResult`].
//!
//! [`RpcAdmission`] bounds the memory retained by announced bodies before any
//! body allocation takes place. Request and response pools are separate so a
//! handler holding its admitted request cannot deadlock while reserving space
//! for its response.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tracing::Level;
use tracing::debug;
use tracing::trace;

use nexigon_api::Action;
use nexigon_api::types::errors::ActionError;
use nexigon_api::types::errors::ActionResult;

/// Maximum action name size (255 bytes).
pub const MAX_ACTION_NAME_SIZE: usize = 255;

/// Maximum action input size (8 MiB).
pub const MAX_INPUT_SIZE: usize = 8 * 1024 * 1024;

/// Maximum action output size (8 MiB).
pub const MAX_OUTPUT_SIZE: usize = 8 * 1024 * 1024;

/// Default aggregate admitted request/response memory per direction (64 MiB).
pub const DEFAULT_GLOBAL_MEMORY_BYTES: u32 = 64 * 1024 * 1024;

/// Default admitted request/response memory per actor and direction (16 MiB).
pub const DEFAULT_PER_ACTOR_MEMORY_BYTES: u32 = 16 * 1024 * 1024;

/// Default deadline for body admission/read after its length is announced, or
/// for writing one complete serialized frame.
pub const DEFAULT_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest configurable per-frame body deadline (5 minutes).
pub const MAX_BODY_TIMEOUT: Duration = Duration::from_secs(5 * 60);

const READ_CHUNK_SIZE: usize = 16 * 1024;
const STALE_ACTOR_CLEANUP_THRESHOLD: usize = 64;
const MAX_TRACKED_ACTORS: usize = 4_096;
const DEFAULT_ACTOR_KEY: &str = "__unscoped_rpc_actor__";

/// Memory and time limits for RPC messages.
///
/// The memory values apply independently to requests and responses. Keeping
/// directional pools avoids a request-at-capacity deadlocking while its
/// handler creates the response. The aggregate maximum retained RPC body
/// memory is therefore twice `global_memory_bytes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RpcLimits {
    /// Aggregate admitted bytes in each direction.
    pub global_memory_bytes: u32,
    /// Admitted bytes for one actor in each direction.
    pub per_actor_memory_bytes: u32,
    /// Deadline for an announced body read or one complete frame write.
    pub body_timeout: Duration,
}

impl Default for RpcLimits {
    fn default() -> Self {
        Self {
            global_memory_bytes: DEFAULT_GLOBAL_MEMORY_BYTES,
            per_actor_memory_bytes: DEFAULT_PER_ACTOR_MEMORY_BYTES,
            body_timeout: DEFAULT_BODY_TIMEOUT,
        }
    }
}

/// Invalid RPC admission configuration.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RpcLimitsError {
    /// A memory limit is zero.
    #[error("RPC memory limits must be greater than zero")]
    ZeroMemory,
    /// A per-actor limit is greater than the global limit.
    #[error("RPC per-actor memory limit exceeds the global limit")]
    ActorExceedsGlobal,
    /// A memory limit cannot admit one maximum-sized protocol body.
    #[error("RPC memory limits must admit at least one {minimum}-byte body")]
    BelowProtocolMaximum { minimum: usize },
    /// The body timeout is zero.
    #[error("RPC body timeout must be greater than zero")]
    ZeroTimeout,
    /// The body timeout is too large to be an effective resource bound.
    #[error("RPC body timeout exceeds the maximum of {maximum:?}")]
    TimeoutTooLarge { maximum: Duration },
    /// A memory limit exceeds Tokio's semaphore representation.
    #[error("RPC memory limit exceeds the supported semaphore capacity")]
    MemoryTooLarge,
}

/// Shared weighted memory admission for RPC request and response bodies.
#[derive(Clone, Debug)]
pub struct RpcAdmission {
    request: WeightedPool,
    response: WeightedPool,
    body_timeout: Duration,
}

impl RpcAdmission {
    /// Create an admission controller with validated limits.
    pub fn new(limits: RpcLimits) -> Result<Self, RpcLimitsError> {
        if limits.global_memory_bytes == 0 || limits.per_actor_memory_bytes == 0 {
            return Err(RpcLimitsError::ZeroMemory);
        }
        if limits.per_actor_memory_bytes > limits.global_memory_bytes {
            return Err(RpcLimitsError::ActorExceedsGlobal);
        }
        let minimum = MAX_INPUT_SIZE.max(MAX_OUTPUT_SIZE);
        if usize::try_from(limits.per_actor_memory_bytes).unwrap_or(0) < minimum {
            return Err(RpcLimitsError::BelowProtocolMaximum { minimum });
        }
        if limits.body_timeout.is_zero() {
            return Err(RpcLimitsError::ZeroTimeout);
        }
        if limits.body_timeout > MAX_BODY_TIMEOUT {
            return Err(RpcLimitsError::TimeoutTooLarge {
                maximum: MAX_BODY_TIMEOUT,
            });
        }
        let global_memory_bytes = usize::try_from(limits.global_memory_bytes)
            .map_err(|_| RpcLimitsError::MemoryTooLarge)?;
        if global_memory_bytes > Semaphore::MAX_PERMITS {
            return Err(RpcLimitsError::MemoryTooLarge);
        }
        Ok(Self {
            request: WeightedPool::new(limits.global_memory_bytes, limits.per_actor_memory_bytes),
            response: WeightedPool::new(limits.global_memory_bytes, limits.per_actor_memory_bytes),
            body_timeout: limits.body_timeout,
        })
    }

    /// Effective limits for this controller.
    pub fn limits(&self) -> RpcLimits {
        RpcLimits {
            global_memory_bytes: self.request.global_capacity,
            per_actor_memory_bytes: self.request.actor_capacity,
            body_timeout: self.body_timeout,
        }
    }

    /// Globally available request permits, useful for health/metrics reporting.
    pub fn available_request_bytes(&self) -> usize {
        self.request.global.available_permits()
    }

    /// Globally available response permits, useful for health/metrics reporting.
    pub fn available_response_bytes(&self) -> usize {
        self.response.global.available_permits()
    }

    fn deadline(&self) -> Instant {
        // Configuration is validated as non-zero and at most five minutes.
        Instant::now() + self.body_timeout
    }
}

impl Default for RpcAdmission {
    fn default() -> Self {
        Self::new(RpcLimits::default()).expect("default RPC limits are valid")
    }
}

#[derive(Clone, Debug)]
struct WeightedPool {
    global: Arc<Semaphore>,
    global_capacity: u32,
    actor_capacity: u32,
    actors: Arc<Mutex<HashMap<String, Weak<Semaphore>>>>,
    /// Excess concurrent identities share one budget rather than growing the
    /// actor registry without bound. Normal actors return to dedicated budgets
    /// as soon as stale entries can be reclaimed.
    overflow_actor: Arc<Semaphore>,
}

impl WeightedPool {
    fn new(global_capacity: u32, actor_capacity: u32) -> Self {
        Self {
            global: Arc::new(Semaphore::new(global_capacity as usize)),
            global_capacity,
            actor_capacity,
            actors: Arc::new(Mutex::new(HashMap::new())),
            overflow_actor: Arc::new(Semaphore::new(actor_capacity as usize)),
        }
    }

    async fn acquire(&self, actor_key: &str, bytes: u32) -> MemoryPermit {
        if bytes == 0 {
            return MemoryPermit::default();
        }

        let actor = {
            let mut actors = self
                .actors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if actors.len() >= STALE_ACTOR_CLEANUP_THRESHOLD {
                actors.retain(|_, semaphore| semaphore.strong_count() != 0);
            }
            if let Some(semaphore) = actors.get(actor_key).and_then(Weak::upgrade) {
                semaphore
            } else if actors.len() >= MAX_TRACKED_ACTORS {
                self.overflow_actor.clone()
            } else {
                let semaphore = Arc::new(Semaphore::new(self.actor_capacity as usize));
                actors.insert(actor_key.to_owned(), Arc::downgrade(&semaphore));
                semaphore
            }
        };

        // Per-actor admission comes first. A noisy actor waiting for global
        // capacity then retains at most its own configured share.
        let actor = actor
            .acquire_many_owned(bytes)
            .await
            .expect("RPC admission semaphores are never closed");
        let global = self
            .global
            .clone()
            .acquire_many_owned(bytes)
            .await
            .expect("RPC admission semaphores are never closed");
        MemoryPermit {
            global: Some(global),
            actor: Some(actor),
        }
    }
}

#[derive(Debug, Default)]
struct MemoryPermit {
    global: Option<OwnedSemaphorePermit>,
    actor: Option<OwnedSemaphorePermit>,
}

impl MemoryPermit {
    fn shrink_to(self, bytes: u32) -> Self {
        if bytes == 0 {
            return Self::default();
        }
        fn retain(
            mut permit: Option<OwnedSemaphorePermit>,
            bytes: u32,
        ) -> Option<OwnedSemaphorePermit> {
            let retained = permit
                .as_mut()
                .and_then(|permit| permit.split(bytes as usize))
                .expect("retained RPC bytes never exceed the reserved maximum");
            drop(permit);
            Some(retained)
        }
        Self {
            global: retain(self.global, bytes),
            actor: retain(self.actor, bytes),
        }
    }
}

fn default_admission() -> &'static RpcAdmission {
    static DEFAULT: OnceLock<RpcAdmission> = OnceLock::new();
    DEFAULT.get_or_init(RpcAdmission::default)
}

/// Execute an action over the given transport.
#[tracing::instrument(level = Level::DEBUG, skip_all, fields(action.name = A::NAME))]
pub async fn execute<A: Action, R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    action: &A,
    rx: R,
    tx: W,
) -> Result<Result<A::Output, ActionError>, ExecuteError> {
    execute_with_admission(action, rx, tx, default_admission(), DEFAULT_ACTOR_KEY).await
}

/// Execute an action with explicit shared memory admission and actor identity.
#[tracing::instrument(level = Level::DEBUG, skip_all, fields(action.name = A::NAME))]
pub async fn execute_with_admission<A: Action, R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    action: &A,
    mut rx: R,
    mut tx: W,
    admission: &RpcAdmission,
    actor_key: &str,
) -> Result<Result<A::Output, ActionError>, ExecuteError> {
    debug!("executing action");
    validate_action_name(A::NAME).map_err(ExecuteError::ActionNameTooLarge)?;
    let name_size = u16::try_from(A::NAME.len())
        .map_err(|_| ExecuteError::ActionNameTooLarge(A::NAME.len()))?;

    let write_deadline = admission.deadline();
    let request_permit = tokio::time::timeout_at(
        write_deadline,
        admission.request.acquire(
            actor_key,
            u32::try_from(MAX_INPUT_SIZE).expect("input limit fits u32"),
        ),
    )
    .await
    .map_err(|_| ExecuteError::WriteTimeout)?;
    let input = match serialize_bounded(action, MAX_INPUT_SIZE) {
        Ok(input) => input,
        Err(BoundedSerializationError::TooLarge(size)) => {
            return Err(ExecuteError::InputTooLarge(size));
        }
        Err(BoundedSerializationError::Allocation) => {
            return Err(ExecuteError::BufferAllocation);
        }
        Err(BoundedSerializationError::Serialization(error)) => {
            return Err(ExecuteError::Serialization(error));
        }
    };
    if Instant::now() >= write_deadline {
        return Err(ExecuteError::WriteTimeout);
    }
    let input_size =
        u32::try_from(input.len()).map_err(|_| ExecuteError::InputTooLarge(input.len()))?;
    let request_permit = request_permit.shrink_to(input_size);

    let (_, output) = tokio::try_join!(
        async {
            write_all_until(&mut tx, &name_size.to_be_bytes(), write_deadline)
                .await
                .map_err(map_execute_write)?;
            write_all_until(&mut tx, A::NAME.as_bytes(), write_deadline)
                .await
                .map_err(map_execute_write)?;
            write_all_until(&mut tx, &input_size.to_be_bytes(), write_deadline)
                .await
                .map_err(map_execute_write)?;
            write_all_until(&mut tx, &input, write_deadline)
                .await
                .map_err(map_execute_write)?;
            flush_until(&mut tx, write_deadline)
                .await
                .map_err(map_execute_write)?;
            drop(request_permit);
            trace!("done sending action");
            Ok(())
        },
        async {
            let mut output_size = [0u8; 4];
            // Waiting for the response header includes action execution and is
            // deliberately not a body timeout. The deadline starts only once
            // the peer announces a body length.
            rx.read_exact(&mut output_size)
                .await
                .map_err(ExecuteError::Read)?;
            let output_size = u32::from_be_bytes(output_size);
            trace!(output_size);
            if output_size as usize > MAX_OUTPUT_SIZE {
                return Err(ExecuteError::OutputTooLarge(output_size as usize));
            }
            let read_deadline = admission.deadline();
            let permit = tokio::time::timeout_at(
                read_deadline,
                admission.response.acquire(actor_key, output_size),
            )
            .await
            .map_err(|_| ExecuteError::ReadTimeout)?;
            let output = read_body(&mut rx, output_size as usize, read_deadline)
                .await
                .map_err(map_execute_body_read)?;
            trace!("done receiving output");
            Ok(AdmittedBody {
                bytes: output,
                _permit: permit,
            })
        }
    )?;
    serde_json::from_slice::<ActionResult<A::Output>>(&output.bytes)
        .map_err(ExecuteError::MalformedOutput)
        .map(Into::into)
}

fn validate_action_name(name: &str) -> Result<(), usize> {
    if name.len() > MAX_ACTION_NAME_SIZE {
        Err(name.len())
    } else {
        Ok(())
    }
}

/// Error executing an action over a transport.
#[derive(Debug, Error)]
pub enum ExecuteError {
    /// Error reading from the transport.
    #[error("error reading from transport")]
    Read(#[source] io::Error),
    /// Reading the framed response exceeded the body deadline.
    #[error("timed out reading from transport")]
    ReadTimeout,
    /// Error writing to the transport.
    #[error("error writing to transport")]
    Write(#[source] io::Error),
    /// Writing the framed request exceeded the body deadline.
    #[error("timed out writing to transport")]
    WriteTimeout,
    /// Error serializing the action input.
    #[error("error serializing action input")]
    Serialization(#[source] serde_json::Error),
    /// A body buffer could not be allocated.
    #[error("unable to allocate RPC body buffer")]
    BufferAllocation,
    /// Malformed output.
    #[error("malformed output")]
    MalformedOutput(#[source] serde_json::Error),
    /// The action name exceeds the protocol maximum.
    #[error("action name exceeds maximum size ({0} > {MAX_ACTION_NAME_SIZE})")]
    ActionNameTooLarge(usize),
    /// The serialized action input exceeds the protocol maximum.
    #[error("action input exceeds maximum size ({0} > {MAX_INPUT_SIZE})")]
    InputTooLarge(usize),
    /// Output exceeds maximum size.
    #[error("output exceeds maximum size ({0} > {MAX_OUTPUT_SIZE})")]
    OutputTooLarge(usize),
}

/// Read an action from the given transport using process-wide default admission.
#[tracing::instrument(level = Level::DEBUG, skip_all)]
pub async fn read_action<R: AsyncRead + Unpin>(rx: R) -> Result<SerializedAction, ReadError> {
    read_action_with_admission(rx, default_admission(), DEFAULT_ACTOR_KEY).await
}

/// Read an action using explicit shared memory admission and actor identity.
#[tracing::instrument(level = Level::DEBUG, skip_all)]
pub async fn read_action_with_admission<R: AsyncRead + Unpin>(
    mut rx: R,
    admission: &RpcAdmission,
    actor_key: &str,
) -> Result<SerializedAction, ReadError> {
    let header = read_action_header(&mut rx).await?;
    header.read_body(&mut rx, admission, actor_key).await
}

/// Read and validate only an action header.
///
/// This performs no body allocation and acquires no body-memory permit. A
/// server can therefore make its actor/rate admission decision before calling
/// [`ActionHeader::read_body`] or [`ActionHeader::discard_body`].
#[tracing::instrument(level = Level::DEBUG, skip_all)]
pub async fn read_action_header<R: AsyncRead + Unpin>(
    mut rx: R,
) -> Result<ActionHeader, ReadError> {
    debug!("receiving action");
    let mut name_size = [0u8; 2];
    rx.read_exact(&mut name_size)
        .await
        .map_err(ReadError::Read)?;
    let name_size = u16::from_be_bytes(name_size);
    trace!(name_size);
    if name_size as usize > MAX_ACTION_NAME_SIZE {
        return Err(ReadError::ActionNameTooLarge(name_size as usize));
    }
    let mut name = vec![0u8; name_size as usize];
    rx.read_exact(&mut name).await.map_err(ReadError::Read)?;
    let name = String::from_utf8(name).map_err(ReadError::InvalidActionName)?;
    trace!(name);

    let mut input_size = [0u8; 4];
    rx.read_exact(&mut input_size)
        .await
        .map_err(ReadError::Read)?;
    let input_size = u32::from_be_bytes(input_size);
    if input_size as usize > MAX_INPUT_SIZE {
        return Err(ReadError::ActionInputTooLarge(input_size as usize));
    }

    Ok(ActionHeader { name, input_size })
}

/// A validated RPC request header whose body has not yet been consumed.
#[derive(Debug)]
pub struct ActionHeader {
    /// Action name.
    pub name: String,
    input_size: u32,
}

impl ActionHeader {
    /// Announced input size in bytes.
    pub fn input_size(&self) -> u32 {
        self.input_size
    }

    /// Admit, incrementally read, and retain the announced request body.
    pub async fn read_body<R: AsyncRead + Unpin>(
        self,
        mut rx: R,
        admission: &RpcAdmission,
        actor_key: &str,
    ) -> Result<SerializedAction, ReadError> {
        let Self { name, input_size } = self;
        let deadline = admission.deadline();

        // Admission precedes every body allocation. The permit is stored in
        // the returned action so dispatch memory remains charged until
        // dispatch drops the serialized input, including on cancellation.
        let permit =
            tokio::time::timeout_at(deadline, admission.request.acquire(actor_key, input_size))
                .await
                .map_err(|_| ReadError::Timeout)?;
        let input = read_body(&mut rx, input_size as usize, deadline)
            .await
            .map_err(map_action_body_read)?;
        debug!(action_name = name, "action has been received");
        Ok(SerializedAction {
            name,
            input,
            _permit: permit,
        })
    }

    /// Incrementally discard a rejected request body under the body deadline.
    ///
    /// Draining preserves framing for the next request without allocating the
    /// attacker-announced body or charging scarce body-memory permits.
    pub async fn discard_body<R: AsyncRead + Unpin>(
        self,
        mut rx: R,
        admission: &RpcAdmission,
    ) -> Result<(), ReadError> {
        discard_body(&mut rx, self.input_size as usize, admission.deadline())
            .await
            .map_err(map_action_body_read)
    }
}

/// Serialized action. Its memory admission permit is retained until drop.
pub struct SerializedAction {
    name: String,
    input: Vec<u8>,
    _permit: MemoryPermit,
}

impl SerializedAction {
    /// Action name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Serialized JSON input.
    pub fn input(&self) -> &[u8] {
        &self.input
    }
}

impl std::fmt::Debug for SerializedAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SerializedAction")
            .field("name", &self.name)
            .field("input_len", &self.input.len())
            .finish_non_exhaustive()
    }
}

/// Error reading an action.
#[derive(Debug, Error)]
pub enum ReadError {
    /// Error reading from the transport.
    #[error("error reading from transport")]
    Read(#[source] io::Error),
    /// Reading the framed request exceeded the body deadline.
    #[error("timed out reading action")]
    Timeout,
    /// A body buffer could not be allocated.
    #[error("unable to allocate action input buffer")]
    BufferAllocation,
    /// Action name is not valid UTF-8.
    #[error("action name is not valid UTF-8")]
    InvalidActionName(#[source] std::string::FromUtf8Error),
    /// Action name exceeds maximum size.
    #[error("action name exceeds maximum size ({0} > {MAX_ACTION_NAME_SIZE})")]
    ActionNameTooLarge(usize),
    /// Action input exceeds maximum size.
    #[error("action input exceeds maximum size ({0} > {MAX_INPUT_SIZE})")]
    ActionInputTooLarge(usize),
}

/// Write an action result using process-wide default admission.
#[tracing::instrument(level = Level::DEBUG, skip_all)]
pub async fn write_action_result<
    T: Serialize + ::sidex_serde::adapter::SidexType,
    W: AsyncWrite + Unpin,
>(
    result: ActionResult<T>,
    tx: W,
) -> Result<(), WriteError> {
    write_action_result_with_admission(result, tx, default_admission(), DEFAULT_ACTOR_KEY).await
}

/// Write an action result using explicit shared memory admission.
#[tracing::instrument(level = Level::DEBUG, skip_all)]
pub async fn write_action_result_with_admission<
    T: Serialize + ::sidex_serde::adapter::SidexType,
    W: AsyncWrite + Unpin,
>(
    result: ActionResult<T>,
    mut tx: W,
    admission: &RpcAdmission,
    actor_key: &str,
) -> Result<(), WriteError> {
    let deadline = admission.deadline();
    // The final size is not known before serialization. Reserve the protocol
    // maximum first, serialize into a hard-bounded fallible writer, then return
    // unused permits. Concurrent serializers therefore remain bounded too.
    let permit = tokio::time::timeout_at(
        deadline,
        admission.response.acquire(
            actor_key,
            u32::try_from(MAX_OUTPUT_SIZE).expect("output limit fits u32"),
        ),
    )
    .await
    .map_err(|_| WriteError::Timeout)?;
    let result = match serialize_bounded(&result, MAX_OUTPUT_SIZE) {
        Ok(result) => result,
        Err(BoundedSerializationError::TooLarge(size)) => {
            return Err(WriteError::OutputTooLarge(size));
        }
        Err(BoundedSerializationError::Allocation) => {
            return Err(WriteError::BufferAllocation);
        }
        Err(BoundedSerializationError::Serialization(error)) => {
            return Err(WriteError::Serialization(error));
        }
    };
    if Instant::now() >= deadline {
        return Err(WriteError::Timeout);
    }
    let result_size =
        u32::try_from(result.len()).map_err(|_| WriteError::OutputTooLarge(result.len()))?;
    let permit = permit.shrink_to(result_size);

    debug!("sending action result");
    write_all_until(&mut tx, &result_size.to_be_bytes(), deadline)
        .await
        .map_err(map_result_write)?;
    write_all_until(&mut tx, &result, deadline)
        .await
        .map_err(map_result_write)?;
    flush_until(&mut tx, deadline)
        .await
        .map_err(map_result_write)?;
    drop(permit);
    debug!("done sending action result");
    Ok(())
}

/// Error writing an action result.
#[derive(Debug, Error)]
pub enum WriteError {
    /// Error writing to the transport.
    #[error("error writing to transport")]
    Write(#[source] io::Error),
    /// Writing or admission exceeded the body deadline.
    #[error("timed out writing action result")]
    Timeout,
    /// Error serializing the action result.
    #[error("error serializing action result")]
    Serialization(#[source] serde_json::Error),
    /// A body buffer could not be allocated.
    #[error("unable to allocate action output buffer")]
    BufferAllocation,
    /// The serialized result exceeds the protocol maximum.
    #[error("action output exceeds maximum size ({0} > {MAX_OUTPUT_SIZE})")]
    OutputTooLarge(usize),
}

#[derive(Debug)]
struct AdmittedBody {
    bytes: Vec<u8>,
    _permit: MemoryPermit,
}

#[derive(Debug)]
enum TimedIoError {
    Io(io::Error),
    Timeout,
}

async fn write_all_until<W: AsyncWrite + Unpin>(
    writer: &mut W,
    buffer: &[u8],
    deadline: Instant,
) -> Result<(), TimedIoError> {
    tokio::time::timeout_at(deadline, writer.write_all(buffer))
        .await
        .map_err(|_| TimedIoError::Timeout)?
        .map_err(TimedIoError::Io)
}

async fn flush_until<W: AsyncWrite + Unpin>(
    writer: &mut W,
    deadline: Instant,
) -> Result<(), TimedIoError> {
    tokio::time::timeout_at(deadline, writer.flush())
        .await
        .map_err(|_| TimedIoError::Timeout)?
        .map_err(TimedIoError::Io)
}

async fn read_body<R: AsyncRead + Unpin>(
    reader: &mut R,
    size: usize,
    deadline: Instant,
) -> Result<Vec<u8>, BodyReadError> {
    let mut body = Vec::new();
    let mut chunk = [0u8; READ_CHUNK_SIZE];
    while body.len() < size {
        let remaining = size - body.len();
        let chunk_size = remaining.min(chunk.len());
        body.try_reserve_exact(chunk_size)
            .map_err(|_| BodyReadError::Allocation)?;
        let read = tokio::time::timeout_at(deadline, reader.read(&mut chunk[..chunk_size]))
            .await
            .map_err(|_| BodyReadError::Timeout)?
            .map_err(BodyReadError::Io)?;
        if read == 0 {
            return Err(BodyReadError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "RPC body ended before its announced length",
            )));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(body)
}

async fn discard_body<R: AsyncRead + Unpin>(
    reader: &mut R,
    size: usize,
    deadline: Instant,
) -> Result<(), BodyReadError> {
    let mut discarded = 0usize;
    let mut chunk = [0u8; READ_CHUNK_SIZE];
    while discarded < size {
        let chunk_size = (size - discarded).min(chunk.len());
        let read = tokio::time::timeout_at(deadline, reader.read(&mut chunk[..chunk_size]))
            .await
            .map_err(|_| BodyReadError::Timeout)?
            .map_err(BodyReadError::Io)?;
        if read == 0 {
            return Err(BodyReadError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "RPC body ended before its announced length",
            )));
        }
        discarded += read;
    }
    Ok(())
}

#[derive(Debug)]
enum BodyReadError {
    Io(io::Error),
    Timeout,
    Allocation,
}

fn map_action_body_read(error: BodyReadError) -> ReadError {
    match error {
        BodyReadError::Io(error) => ReadError::Read(error),
        BodyReadError::Timeout => ReadError::Timeout,
        BodyReadError::Allocation => ReadError::BufferAllocation,
    }
}

fn map_execute_body_read(error: BodyReadError) -> ExecuteError {
    match error {
        BodyReadError::Io(error) => ExecuteError::Read(error),
        BodyReadError::Timeout => ExecuteError::ReadTimeout,
        BodyReadError::Allocation => ExecuteError::BufferAllocation,
    }
}

fn map_execute_write(error: TimedIoError) -> ExecuteError {
    match error {
        TimedIoError::Io(error) => ExecuteError::Write(error),
        TimedIoError::Timeout => ExecuteError::WriteTimeout,
    }
}

fn map_result_write(error: TimedIoError) -> WriteError {
    match error {
        TimedIoError::Io(error) => WriteError::Write(error),
        TimedIoError::Timeout => WriteError::Timeout,
    }
}

#[derive(Debug)]
enum BoundedSerializationError {
    TooLarge(usize),
    Allocation,
    Serialization(serde_json::Error),
}

fn serialize_bounded<T: Serialize>(
    value: &T,
    limit: usize,
) -> Result<Vec<u8>, BoundedSerializationError> {
    let mut writer = BoundedVecWriter::new(limit);
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(writer.buffer),
        Err(error) => match writer.failure {
            Some(BufferFailure::TooLarge(size)) => Err(BoundedSerializationError::TooLarge(size)),
            Some(BufferFailure::Allocation) => Err(BoundedSerializationError::Allocation),
            None => Err(BoundedSerializationError::Serialization(error)),
        },
    }
}

struct BoundedVecWriter {
    buffer: Vec<u8>,
    limit: usize,
    failure: Option<BufferFailure>,
}

impl BoundedVecWriter {
    fn new(limit: usize) -> Self {
        Self {
            buffer: Vec::new(),
            limit,
            failure: None,
        }
    }
}

#[derive(Clone, Copy)]
enum BufferFailure {
    TooLarge(usize),
    Allocation,
}

impl io::Write for BoundedVecWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let size = self.buffer.len().saturating_add(bytes.len());
        if size > self.limit {
            self.failure = Some(BufferFailure::TooLarge(size));
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "serialized RPC body exceeds its maximum",
            ));
        }
        if self.buffer.try_reserve_exact(bytes.len()).is_err() {
            self.failure = Some(BufferFailure::Allocation);
            return Err(io::Error::other("unable to allocate serialized RPC body"));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::task::Context;
    use std::task::Poll;
    use std::time::Duration;

    use nexigon_api::types::errors::ActionResult;
    use nexigon_api::types::jwt::Jwt;
    use nexigon_api::types::users::CompleteUserPasswordResetAction;
    use nexigon_api::types::users::CompleteUserPasswordResetOutput;
    use serde::Deserialize;
    use serde::Serialize;
    use serde::Serializer;
    use tokio::io::AsyncWrite;
    use tokio::io::duplex;
    use tokio::io::split;
    use tokio::task::yield_now;
    use tracing::Level;
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    const JWT_SENTINEL: &str = "sentinel.jwt.must-not-appear";
    const PASSWORD_SENTINEL: &str = "sentinel-password-must-not-appear";

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

        let (client, mut server) = duplex(4096);
        let (client_rx, client_tx) = split(client);
        let server_task = tokio::spawn(async move {
            let serialized = read_action(&mut server).await.expect("read action");
            let input =
                String::from_utf8(serialized.input().to_vec()).expect("JSON input is UTF-8");
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
            .with_subscriber(subscriber)
            .await
            .expect("RPC transport succeeds")
            .expect("action succeeds");
        server_task.await.expect("server task succeeds");
        assert!(matches!(result, CompleteUserPasswordResetOutput::Completed));

        let output = output.contents();
        assert!(output.contains("users_CompletePasswordReset"));
        for sentinel in [JWT_SENTINEL, PASSWORD_SENTINEL] {
            assert!(!output.contains(sentinel), "telemetry leaked {sentinel:?}");
        }
    }

    #[test]
    fn limits_reject_invalid_or_semantically_incompatible_values() {
        let limits = RpcLimits {
            body_timeout: Duration::ZERO,
            ..RpcLimits::default()
        };
        assert_eq!(
            RpcAdmission::new(limits).unwrap_err(),
            RpcLimitsError::ZeroTimeout
        );

        let limits = RpcLimits {
            per_actor_memory_bytes: DEFAULT_GLOBAL_MEMORY_BYTES + 1,
            ..RpcLimits::default()
        };
        assert_eq!(
            RpcAdmission::new(limits).unwrap_err(),
            RpcLimitsError::ActorExceedsGlobal
        );

        let limits = RpcLimits {
            per_actor_memory_bytes: MAX_INPUT_SIZE as u32 - 1,
            ..RpcLimits::default()
        };
        assert!(matches!(
            RpcAdmission::new(limits),
            Err(RpcLimitsError::BelowProtocolMaximum { .. })
        ));

        let limits = RpcLimits {
            body_timeout: MAX_BODY_TIMEOUT + Duration::from_secs(1),
            ..RpcLimits::default()
        };
        assert!(matches!(
            RpcAdmission::new(limits),
            Err(RpcLimitsError::TimeoutTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn exact_name_and_input_limits_are_accepted() {
        let admission = RpcAdmission::default();
        let (mut client, mut server) = duplex(MAX_INPUT_SIZE + 1024);
        let send = tokio::spawn(async move {
            client
                .write_all(&(MAX_ACTION_NAME_SIZE as u16).to_be_bytes())
                .await
                .unwrap();
            client
                .write_all(&vec![b'a'; MAX_ACTION_NAME_SIZE])
                .await
                .unwrap();
            client
                .write_all(&(MAX_INPUT_SIZE as u32).to_be_bytes())
                .await
                .unwrap();
            client.write_all(&vec![b'x'; MAX_INPUT_SIZE]).await.unwrap();
        });
        let action = read_action_with_admission(&mut server, &admission, "actor")
            .await
            .expect("boundary request accepted");
        assert_eq!(action.name().len(), MAX_ACTION_NAME_SIZE);
        assert_eq!(action.input().len(), MAX_INPUT_SIZE);
        assert_eq!(
            admission.available_request_bytes(),
            DEFAULT_GLOBAL_MEMORY_BYTES as usize - MAX_INPUT_SIZE
        );
        drop(action);
        assert_eq!(
            admission.available_request_bytes(),
            DEFAULT_GLOBAL_MEMORY_BYTES as usize
        );
        send.await.unwrap();
    }

    #[tokio::test]
    async fn over_limit_name_and_input_headers_are_rejected_before_body() {
        let admission = RpcAdmission::default();
        let (mut client, mut server) = duplex(1024);
        client.write_all(&256u16.to_be_bytes()).await.unwrap();
        assert!(matches!(
            read_action_with_admission(&mut server, &admission, "actor").await,
            Err(ReadError::ActionNameTooLarge(256))
        ));

        let (mut client, mut server) = duplex(1024);
        client.write_all(&1u16.to_be_bytes()).await.unwrap();
        client.write_all(b"a").await.unwrap();
        client
            .write_all(&((MAX_INPUT_SIZE as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        assert!(matches!(
            read_action_with_admission(&mut server, &admission, "actor").await,
            Err(ReadError::ActionInputTooLarge(size)) if size == MAX_INPUT_SIZE + 1
        ));
        assert_eq!(
            admission.available_request_bytes(),
            DEFAULT_GLOBAL_MEMORY_BYTES as usize
        );
    }

    #[tokio::test]
    async fn header_decision_precedes_body_allocation_and_rejected_body_can_be_drained() {
        let admission = RpcAdmission::default();
        let before = admission.available_request_bytes();
        let (mut client, mut server) = duplex(1024);
        client.write_all(&1u16.to_be_bytes()).await.unwrap();
        client.write_all(b"a").await.unwrap();
        client.write_all(&4u32.to_be_bytes()).await.unwrap();
        client.write_all(b"junk").await.unwrap();
        client.write_all(&1u16.to_be_bytes()).await.unwrap();
        client.write_all(b"b").await.unwrap();
        client.write_all(&2u32.to_be_bytes()).await.unwrap();
        client.write_all(b"{}").await.unwrap();

        let rejected = read_action_header(&mut server).await.unwrap();
        assert_eq!(rejected.name, "a");
        assert_eq!(rejected.input_size(), 4);
        assert_eq!(admission.available_request_bytes(), before);
        rejected
            .discard_body(&mut server, &admission)
            .await
            .unwrap();
        assert_eq!(admission.available_request_bytes(), before);

        let accepted = read_action_with_admission(&mut server, &admission, "actor")
            .await
            .unwrap();
        assert_eq!(accepted.name(), "b");
        assert_eq!(accepted.input(), b"{}");
    }

    #[tokio::test]
    async fn serialized_action_debug_omits_body_contents() {
        let sentinel = b"secret-body-sentinel";
        let (mut client, mut server) = duplex(128);
        client.write_all(&1u16.to_be_bytes()).await.unwrap();
        client.write_all(b"a").await.unwrap();
        client
            .write_all(&(sentinel.len() as u32).to_be_bytes())
            .await
            .unwrap();
        client.write_all(sentinel).await.unwrap();
        let action = read_action(&mut server).await.unwrap();
        let debug = format!("{action:?}");
        assert!(debug.contains("input_len"));
        assert!(!debug.contains("secret-body-sentinel"));
    }

    #[tokio::test]
    async fn invalid_utf8_name_is_a_protocol_error() {
        let (mut client, mut server) = duplex(16);
        client.write_all(&1u16.to_be_bytes()).await.unwrap();
        client.write_all(&[0xff]).await.unwrap();
        client.write_all(&0u32.to_be_bytes()).await.unwrap();
        assert!(matches!(
            read_action_header(&mut server).await,
            Err(ReadError::InvalidActionName(_))
        ));
    }

    #[test]
    fn bounded_serialization_accepts_exact_size_and_rejects_one_more() {
        // JSON string serialization adds two quote bytes.
        let exact = "x".repeat(MAX_OUTPUT_SIZE - 2);
        assert_eq!(
            serialize_bounded(&exact, MAX_OUTPUT_SIZE).unwrap().len(),
            MAX_OUTPUT_SIZE
        );
        let over = "x".repeat(MAX_OUTPUT_SIZE - 1);
        assert!(matches!(
            serialize_bounded(&over, MAX_OUTPUT_SIZE),
            Err(BoundedSerializationError::TooLarge(size)) if size > MAX_OUTPUT_SIZE
        ));
    }

    #[tokio::test]
    async fn outbound_action_name_limits_are_symmetric() {
        assert_eq!(
            validate_action_name(&"n".repeat(MAX_ACTION_NAME_SIZE)),
            Ok(())
        );
        assert_eq!(
            validate_action_name(&"n".repeat(MAX_ACTION_NAME_SIZE + 1)),
            Err(MAX_ACTION_NAME_SIZE + 1)
        );

        let (client, mut server) = duplex(1024);
        let (rx, tx) = split(client);
        let server_task = tokio::spawn(async move {
            let action = read_action(&mut server).await.unwrap();
            assert_eq!(action.name().len(), MAX_ACTION_NAME_SIZE);
            write_action_result(ActionResult::Ok(()), &mut server)
                .await
                .unwrap();
        });
        execute(&MaximumNameAction, rx, tx)
            .await
            .expect("maximum name is accepted")
            .expect("server returned success");
        server_task.await.unwrap();

        assert!(matches!(
            execute(
                &OverlongNameAction,
                tokio::io::empty(),
                tokio::io::sink()
            )
            .await,
            Err(ExecuteError::ActionNameTooLarge(size)) if size == MAX_ACTION_NAME_SIZE + 1
        ));
    }

    #[tokio::test]
    async fn execute_accepts_exact_input_limit_and_rejects_one_more() {
        let action = StringAction("x".repeat(MAX_INPUT_SIZE - 2));
        let (client, mut server) = duplex(64 * 1024);
        let (rx, tx) = split(client);
        let server_task = tokio::spawn(async move {
            let action = read_action(&mut server).await.unwrap();
            assert_eq!(action.input().len(), MAX_INPUT_SIZE);
            write_action_result(ActionResult::Ok(()), &mut server)
                .await
                .unwrap();
        });
        execute(&action, rx, tx)
            .await
            .expect("exact maximum input is accepted")
            .expect("server returned success");
        server_task.await.unwrap();

        let over = StringAction("x".repeat(MAX_INPUT_SIZE - 1));
        assert!(matches!(
            execute(&over, tokio::io::empty(), tokio::io::sink()).await,
            Err(ExecuteError::InputTooLarge(size)) if size > MAX_INPUT_SIZE
        ));
    }

    #[tokio::test]
    async fn result_writer_accepts_exact_output_limit_and_rejects_one_more() {
        let empty_size = serialize_bounded(&ActionResult::Ok(String::new()), MAX_OUTPUT_SIZE)
            .unwrap()
            .len();
        let exact = ActionResult::Ok("x".repeat(MAX_OUTPUT_SIZE - empty_size));
        assert_eq!(
            serialize_bounded(&exact, MAX_OUTPUT_SIZE).unwrap().len(),
            MAX_OUTPUT_SIZE
        );
        let (mut client, server) = duplex(MAX_OUTPUT_SIZE + 16);
        write_action_result(exact, server)
            .await
            .expect("exact maximum output is accepted");
        let mut size = [0u8; 4];
        client.read_exact(&mut size).await.unwrap();
        assert_eq!(u32::from_be_bytes(size) as usize, MAX_OUTPUT_SIZE);
        let mut output = vec![0u8; MAX_OUTPUT_SIZE];
        client.read_exact(&mut output).await.unwrap();

        let over = ActionResult::Ok("x".repeat(MAX_OUTPUT_SIZE - empty_size + 1));
        let error = write_action_result(over, tokio::io::sink())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            WriteError::OutputTooLarge(size) if size > MAX_OUTPUT_SIZE
        ));
    }

    #[tokio::test]
    async fn actor_and_global_weighted_budgets_gate_complete_announcements() {
        let admission = RpcAdmission::new(RpcLimits {
            global_memory_bytes: (2 * MAX_INPUT_SIZE) as u32,
            per_actor_memory_bytes: MAX_INPUT_SIZE as u32,
            ..RpcLimits::default()
        })
        .unwrap();

        let first = read_complete_max_body(&admission, "actor-a").await;
        assert_eq!(admission.available_request_bytes(), MAX_INPUT_SIZE);

        let second_admission = admission.clone();
        let mut second =
            tokio::spawn(async move { read_complete_max_body(&second_admission, "actor-a").await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second)
                .await
                .is_err(),
            "same actor must wait for its weighted permit"
        );

        let other = read_complete_max_body(&admission, "actor-b").await;
        assert_eq!(admission.available_request_bytes(), 0);
        drop(first);
        let second = second.await.expect("admitted task survives");
        drop(second);
        drop(other);
        assert_eq!(admission.available_request_bytes(), 2 * MAX_INPUT_SIZE);
    }

    #[tokio::test]
    async fn concurrent_actor_registry_has_a_hard_metadata_bound() {
        let admission = RpcAdmission::default();
        let mut permits = Vec::with_capacity(MAX_TRACKED_ACTORS + 128);
        for actor in 0..MAX_TRACKED_ACTORS + 128 {
            permits.push(
                admission
                    .request
                    .acquire(&format!("actor-{actor}"), 1)
                    .await,
            );
        }

        assert_eq!(
            admission
                .request
                .actors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            MAX_TRACKED_ACTORS
        );
        drop(permits);
        assert_eq!(
            admission.available_request_bytes(),
            DEFAULT_GLOBAL_MEMORY_BYTES as usize
        );
    }

    async fn read_complete_max_body(admission: &RpcAdmission, actor_key: &str) -> SerializedAction {
        let (mut client, mut server) = duplex(MAX_INPUT_SIZE + 16);
        let sender = tokio::spawn(async move {
            client.write_all(&1u16.to_be_bytes()).await.unwrap();
            client.write_all(b"a").await.unwrap();
            client
                .write_all(&(MAX_INPUT_SIZE as u32).to_be_bytes())
                .await
                .unwrap();
            client.write_all(&vec![b'x'; MAX_INPUT_SIZE]).await.unwrap();
        });
        let action = read_action_with_admission(&mut server, admission, actor_key)
            .await
            .unwrap();
        sender.await.unwrap();
        action
    }

    #[tokio::test]
    async fn output_header_limit_and_truncated_body_are_reported_as_reads() {
        let action = CompleteUserPasswordResetAction::new(
            Jwt::from_string("jwt".to_owned()),
            "password".to_owned(),
        );
        let (client, mut server) = duplex(4096);
        let (rx, tx) = split(client);
        server
            .write_all(&((MAX_OUTPUT_SIZE as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        let error = execute(&action, rx, tx).await.unwrap_err();
        assert!(matches!(error, ExecuteError::OutputTooLarge(size) if size == MAX_OUTPUT_SIZE + 1));

        let (client, mut server) = duplex(4096);
        let (rx, tx) = split(client);
        server.write_all(&4u32.to_be_bytes()).await.unwrap();
        server.write_all(b"{}").await.unwrap();
        server.shutdown().await.unwrap();
        let error = execute(&action, rx, tx).await.unwrap_err();
        assert!(matches!(
            error,
            ExecuteError::Read(error) if error.kind() == io::ErrorKind::UnexpectedEof
        ));
    }

    #[tokio::test]
    async fn malformed_json_is_a_protocol_output_error() {
        let action = CompleteUserPasswordResetAction::new(
            Jwt::from_string("jwt".to_owned()),
            "password".to_owned(),
        );
        let (client, mut server) = duplex(4096);
        let (rx, tx) = split(client);
        server.write_all(&1u32.to_be_bytes()).await.unwrap();
        server.write_all(b"{").await.unwrap();
        let error = execute(&action, rx, tx).await.unwrap_err();
        assert!(matches!(error, ExecuteError::MalformedOutput(_)));
    }

    #[tokio::test]
    async fn failing_writer_is_classified_as_write_error() {
        let action = CompleteUserPasswordResetAction::new(
            Jwt::from_string("jwt".to_owned()),
            "password".to_owned(),
        );
        let error = execute(&action, tokio::io::empty(), FailingWriter)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ExecuteError::Write(error) if error.kind() == io::ErrorKind::BrokenPipe
        ));
    }

    #[tokio::test]
    async fn result_serialization_failure_is_fallible_and_writes_nothing() {
        let (mut client, server) = duplex(1024);
        let error = write_action_result(ActionResult::Ok(FailsSerialization), server)
            .await
            .unwrap_err();
        assert!(matches!(error, WriteError::Serialization(_)));
        drop(error);
        let mut byte = [0u8; 1];
        assert_eq!(client.read(&mut byte).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn input_serialization_failure_is_fallible_and_writes_nothing() {
        let error = execute(
            &FailsSerializationAction,
            tokio::io::empty(),
            tokio::io::sink(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ExecuteError::Serialization(_)));
    }

    #[tokio::test]
    async fn failing_result_writer_is_a_write_error_and_releases_permits() {
        let admission = RpcAdmission::default();
        let before = admission.available_response_bytes();
        let error = write_action_result_with_admission(
            ActionResult::Ok(()),
            FailingWriter,
            &admission,
            "actor",
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            WriteError::Write(error) if error.kind() == io::ErrorKind::BrokenPipe
        ));
        assert_eq!(admission.available_response_bytes(), before);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_result_write_times_out_and_releases_permits() {
        let admission = RpcAdmission::new(RpcLimits {
            body_timeout: Duration::from_secs(5),
            ..RpcLimits::default()
        })
        .unwrap();
        let before = admission.available_response_bytes();
        let write = write_action_result_with_admission(
            ActionResult::Ok(()),
            PendingWriter,
            &admission,
            "actor",
        );
        tokio::pin!(write);
        assert!(matches!(futures_poll_once(&mut write).await, Poll::Pending));
        assert!(admission.available_response_bytes() < before);
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(matches!(write.await, Err(WriteError::Timeout)));
        assert_eq!(admission.available_response_bytes(), before);
    }

    #[tokio::test]
    async fn delayed_response_header_is_not_mistaken_for_a_stalled_body() {
        let admission = RpcAdmission::new(RpcLimits {
            body_timeout: Duration::from_millis(10),
            ..RpcLimits::default()
        })
        .unwrap();
        let action = StringAction("ok".to_owned());
        let (client, mut server) = duplex(4096);
        let (rx, tx) = split(client);
        let server_task = tokio::spawn(async move {
            let _request = read_action(&mut server).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            write_action_result(ActionResult::Ok(()), &mut server)
                .await
                .unwrap();
        });
        execute_with_admission(&action, rx, tx, &admission, "actor")
            .await
            .expect("action execution time is not a body timeout")
            .expect("server returned success");
        server_task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn announced_body_without_payload_times_out_and_releases_permit() {
        let admission = RpcAdmission::new(RpcLimits {
            body_timeout: Duration::from_secs(5),
            ..RpcLimits::default()
        })
        .unwrap();
        let (mut client, mut server) = duplex(1024);
        client.write_all(&1u16.to_be_bytes()).await.unwrap();
        client.write_all(b"a").await.unwrap();
        client
            .write_all(&(MAX_INPUT_SIZE as u32).to_be_bytes())
            .await
            .unwrap();
        let before = admission.available_request_bytes();
        let read = read_action_with_admission(&mut server, &admission, "actor");
        tokio::pin!(read);
        assert!(matches!(futures_poll_once(&mut read).await, Poll::Pending));
        assert_eq!(admission.available_request_bytes(), before - MAX_INPUT_SIZE);
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(matches!(read.await, Err(ReadError::Timeout)));
        assert_eq!(admission.available_request_bytes(), before);
    }

    #[tokio::test(start_paused = true)]
    async fn many_maximum_announcements_are_bounded_and_time_out() {
        const CHANNELS: usize = 8;
        let admission = RpcAdmission::new(RpcLimits {
            body_timeout: Duration::from_secs(5),
            ..RpcLimits::default()
        })
        .unwrap();
        let before = admission.available_request_bytes();
        let mut clients = Vec::new();
        let mut reads = Vec::new();
        for _ in 0..CHANNELS {
            let (mut client, mut server) = duplex(64);
            client.write_all(&1u16.to_be_bytes()).await.unwrap();
            client.write_all(b"a").await.unwrap();
            client
                .write_all(&(MAX_INPUT_SIZE as u32).to_be_bytes())
                .await
                .unwrap();
            clients.push(client);
            let task_admission = admission.clone();
            reads.push(tokio::spawn(async move {
                read_action_with_admission(&mut server, &task_admission, "one-actor").await
            }));
        }
        for _ in 0..CHANNELS {
            yield_now().await;
        }
        assert_eq!(
            admission.available_request_bytes(),
            before - DEFAULT_PER_ACTOR_MEMORY_BYTES as usize,
            "only the per-actor budget may be reserved"
        );

        tokio::time::advance(Duration::from_secs(5)).await;
        for read in reads {
            assert!(matches!(read.await.unwrap(), Err(ReadError::Timeout)));
        }
        assert_eq!(admission.available_request_bytes(), before);
        drop(clients);
    }

    #[tokio::test]
    async fn cancellation_and_disconnect_restore_weighted_permits() {
        let admission = RpcAdmission::default();
        let before = admission.available_request_bytes();
        let (mut client, mut server) = duplex(1024);
        client.write_all(&1u16.to_be_bytes()).await.unwrap();
        client.write_all(b"a").await.unwrap();
        client
            .write_all(&(MAX_INPUT_SIZE as u32).to_be_bytes())
            .await
            .unwrap();
        let task_admission = admission.clone();
        let task = tokio::spawn(async move {
            read_action_with_admission(&mut server, &task_admission, "same-actor").await
        });
        while admission.available_request_bytes() == before {
            yield_now().await;
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(admission.available_request_bytes(), before);

        let (mut client, mut server) = duplex(1024);
        client.write_all(&1u16.to_be_bytes()).await.unwrap();
        client.write_all(b"a").await.unwrap();
        client
            .write_all(&(MAX_INPUT_SIZE as u32).to_be_bytes())
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        assert!(matches!(
            read_action_with_admission(&mut server, &admission, "same-actor").await,
            Err(ReadError::Read(error)) if error.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert_eq!(admission.available_request_bytes(), before);
    }

    async fn futures_poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
        std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut *future).poll(cx))).await
    }

    #[derive(Debug, Deserialize)]
    struct FailsSerialization;

    impl Serialize for FailsSerialization {
        fn serialize<S: Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom(
                "intentional serialization failure",
            ))
        }
    }

    sidex_serde::impl_sidex_type!(FailsSerialization);

    #[derive(Debug, Deserialize)]
    struct FailsSerializationAction;

    impl Serialize for FailsSerializationAction {
        fn serialize<S: Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom(
                "intentional serialization failure",
            ))
        }
    }

    sidex_serde::impl_sidex_type!(FailsSerializationAction);

    impl Action for FailsSerializationAction {
        type Output = ();
        const NAME: &'static str = "test_FailsSerialization";
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct StringAction(String);

    sidex_serde::impl_sidex_type!(StringAction);

    impl Action for StringAction {
        type Output = ();
        const NAME: &'static str = "test_String";
    }

    const NAME_255: &str = concat!(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    const NAME_256: &str = concat!(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );

    #[derive(Debug, Serialize, Deserialize)]
    struct MaximumNameAction;

    sidex_serde::impl_sidex_type!(MaximumNameAction);

    impl Action for MaximumNameAction {
        type Output = ();
        const NAME: &'static str = NAME_255;
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct OverlongNameAction;

    sidex_serde::impl_sidex_type!(OverlongNameAction);

    impl Action for OverlongNameAction {
        type Output = ();
        const NAME: &'static str = NAME_256;
    }

    struct FailingWriter;

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "intentional write failure",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct PendingWriter;

    impl AsyncWrite for PendingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
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
}
