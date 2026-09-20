use super::{
    manifest::ExternalModuleDescriptor,
    process::{self, StderrCapture},
    protocol::{
        self, MessageEvent, MessageEventKind, V6CallError, V6InboundFrame, V6ModuleFrame,
        V6OutboundCoreFrame,
    },
    v6_executor::{V6ExecutionContext, V6ExecutorError, V6TelegramExecutor},
    v6_handles::V6HandleRegistry,
    v6_host::V6HostExecutor,
    v6_registry,
};
use crate::error::ExternalError;
use std::{
    collections::{HashSet, VecDeque},
    ffi::OsString,
    future,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{Duration, Instant, timeout},
};

pub(crate) const V6_CONTROL_QUEUE: usize = 4;
pub(crate) const V6_READER_QUEUE: usize = 8;
pub(crate) const V6_WRITER_QUEUE: usize = 8;
pub(crate) const V6_RPC_QUEUE: usize = 8;
const V6_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const V6_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(5);
const V6_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const V6_RPC_TIMEOUT: Duration = Duration::from_secs(5);
const V6_MAX_ACTIVE_RPCS: usize = V6_RPC_QUEUE;
const V6_MAX_PENDING: usize = 8;

/// Lifecycle response deadline. Queued requests do not receive this deadline
/// until their frame has been written and flushed to the module.
fn lifecycle_timeout() -> Duration {
    V6_LIFECYCLE_TIMEOUT
}

/// Cloneable front end to the single-owner V6 child-process actor.
#[derive(Clone)]
pub(crate) struct V6Process {
    control: mpsc::Sender<Control>,
    descriptor: Arc<ExternalModuleDescriptor>,
    runtime: Arc<Mutex<V6RuntimeState>>,
    handles: Arc<Mutex<V6HandleRegistry>>,
    host_active: Arc<AtomicUsize>,
    pending_handle_releases: Arc<Mutex<Vec<String>>>,
}

#[derive(Debug)]
pub(crate) struct V6StartFailure {
    pub(crate) error: ExternalError,
    pub(crate) diagnostics: process::CrashDiagnostics,
}

fn start_failure(
    descriptor: &ExternalModuleDescriptor,
    error: ExternalError,
    restart_generation: u64,
) -> V6StartFailure {
    let category = match error {
        ExternalError::StateRead | ExternalError::StateWrite => "state",
        ExternalError::InvalidArgument => "invalid_argument",
        _ => "unavailable",
    };
    let diagnostics = process::build_crash_diagnostics_with_context(
        descriptor,
        None,
        &error,
        &StderrCapture::default(),
        process::CrashDiagnosticContext {
            lifecycle_stage: "spawn",
            error_category: category,
            exit_code: None,
            signal: None,
            restart_generation,
        },
    );
    V6StartFailure { error, diagnostics }
}

#[derive(Debug, Clone)]
struct V6RuntimeState {
    status: process::ProcessStatus,
    diagnostic: Option<process::CrashDiagnostics>,
}

fn lock_runtime_state(state: &Mutex<V6RuntimeState>) -> std::sync::MutexGuard<'_, V6RuntimeState> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl V6Process {
    #[allow(dead_code)]
    pub(crate) async fn start(
        descriptor: ExternalModuleDescriptor,
        executor: Arc<dyn V6TelegramExecutor>,
        restart_generation: u64,
    ) -> Result<Self, V6StartFailure> {
        Self::start_with_ledger(
            descriptor,
            executor,
            restart_generation,
            crate::message_provenance::SharedSelfEditLedger::default(),
            None,
            None,
        )
        .await
    }

    pub(crate) async fn start_with_ledger(
        descriptor: ExternalModuleDescriptor,
        executor: Arc<dyn V6TelegramExecutor>,
        restart_generation: u64,
        self_edit_ledger: crate::message_provenance::SharedSelfEditLedger,
        bot_send: Option<Arc<dyn super::v6_host::HostBotSend>>,
        bot_inline: Option<Arc<dyn super::v6_host::HostInlineSurface>>,
    ) -> Result<Self, V6StartFailure> {
        if descriptor.protocol_version != 6
            || !descriptor.entrypoint.starts_with(&descriptor.module_dir)
        {
            return Err(start_failure(
                &descriptor,
                ExternalError::InvalidArgument,
                restart_generation,
            ));
        }

        let state_dir = resolve_v6_module_state_dir(&descriptor.id).ok_or_else(|| {
            start_failure(&descriptor, ExternalError::StateRead, restart_generation)
        })?;
        tokio::fs::create_dir_all(&state_dir).await.map_err(|_| {
            start_failure(&descriptor, ExternalError::StateWrite, restart_generation)
        })?;
        secure_directory(&state_dir).await.map_err(|_| {
            start_failure(&descriptor, ExternalError::StateWrite, restart_generation)
        })?;
        let mut command = Command::new(&descriptor.entrypoint);
        command
            .current_dir(&descriptor.module_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("CLICOLOR_FORCE", "0")
            .env("LAVIS_MODULE_STATE_DIR", &state_dir)
            .env("TERM", "dumb")
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = command.spawn().map_err(|_| {
            start_failure(&descriptor, ExternalError::Unavailable, restart_generation)
        })?;
        let Some(process_group) = child.id() else {
            cleanup_spawned_child(&mut child, None).await;
            return Err(start_failure(
                &descriptor,
                ExternalError::Unavailable,
                restart_generation,
            ));
        };
        let Some(stdin) = child.stdin.take() else {
            cleanup_spawned_child(&mut child, Some(process_group)).await;
            return Err(start_failure(
                &descriptor,
                ExternalError::Unavailable,
                restart_generation,
            ));
        };
        let Some(stdout) = child.stdout.take() else {
            cleanup_spawned_child(&mut child, Some(process_group)).await;
            return Err(start_failure(
                &descriptor,
                ExternalError::Unavailable,
                restart_generation,
            ));
        };
        let Some(stderr) = child.stderr.take() else {
            cleanup_spawned_child(&mut child, Some(process_group)).await;
            return Err(start_failure(
                &descriptor,
                ExternalError::Unavailable,
                restart_generation,
            ));
        };

        let (control_tx, control_rx) = mpsc::channel(V6_CONTROL_QUEUE);
        let (reader_tx, reader_rx) = mpsc::channel(V6_READER_QUEUE);
        let (writer_tx, writer_rx) = mpsc::channel(V6_WRITER_QUEUE);
        let (rpc_tx, rpc_rx) = mpsc::channel(V6_RPC_QUEUE);
        let reader = tokio::spawn(read_stdout(
            BufReader::new(stdout),
            reader_tx,
            descriptor
                .contract_revision
                .unwrap_or(protocol::V6_ALPHA_CONTRACT_REVISION),
        ));
        let writer = tokio::spawn(write_stdin(stdin, writer_rx, rpc_tx.clone()));
        let stderr_capture = Arc::new(Mutex::new(StderrCapture::default()));
        let stderr_drain = tokio::spawn(process::drain_stderr(stderr, stderr_capture.clone()));
        let runtime = Arc::new(Mutex::new(V6RuntimeState {
            status: process::ProcessStatus::Running,
            diagnostic: None,
        }));
        let handles = Arc::new(Mutex::new(V6HandleRegistry::with_generation(
            restart_generation,
        )));
        let host_active = Arc::new(AtomicUsize::new(0));
        let pending_handle_releases = Arc::new(Mutex::new(Vec::new()));
        let host = Arc::new(
            V6HostExecutor::with_registry(
                descriptor.id.clone(),
                descriptor
                    .capabilities
                    .contains(&super::manifest::ExternalCapability::MessageEdit),
                descriptor
                    .capabilities
                    .contains(&super::manifest::ExternalCapability::MessageDelete),
                handles.clone(),
                self_edit_ledger,
            )
            .with_bot_send(
                descriptor
                    .capabilities
                    .contains(&super::manifest::ExternalCapability::MessageSendBot),
                bot_send,
            )
            .with_inline_surface(
                descriptor
                    .contract_revision
                    .unwrap_or(super::protocol::V6_ALPHA_CONTRACT_REVISION)
                    >= super::protocol::V6_INLINE_SINCE,
                bot_inline,
            ),
        );
        tokio::spawn(supervise(
            child,
            process_group,
            descriptor.clone(),
            executor,
            runtime.clone(),
            handles.clone(),
            host_active.clone(),
            pending_handle_releases.clone(),
            host,
            SupervisorIo {
                control_rx,
                reader_rx,
                writer_tx,
                rpc_rx,
                actor_tx: rpc_tx,
                reader,
                writer,
                stderr_drain,
                stderr_capture,
            },
            restart_generation,
        ));
        Ok(Self {
            control: control_tx,
            descriptor: Arc::new(descriptor),
            runtime,
            handles,
            host_active,
            pending_handle_releases,
        })
    }

    pub(crate) fn descriptor(&self) -> &ExternalModuleDescriptor {
        &self.descriptor
    }

    pub(crate) fn status(&self) -> super::process::ProcessStatus {
        lock_runtime_state(&self.runtime).status
    }

    pub(crate) fn diagnostic(&self) -> Option<process::CrashDiagnostics> {
        lock_runtime_state(&self.runtime).diagnostic.clone()
    }

    pub(crate) async fn execute_command(
        &self,
        command: &str,
        arguments: &str,
        argument_entities: &[protocol::CustomEmojiEntity],
    ) -> Result<String, ExternalError> {
        let frame = self
            .execute_with_context(
                protocol::request_id(),
                command.to_owned(),
                arguments.to_owned(),
                argument_entities.to_vec(),
                None,
            )
            .await?;
        match frame {
            V6InboundFrame::Result { text, .. } => Ok(text),
            V6InboundFrame::Error { message, .. } => Err(ExternalError::ModuleError(message)),
            _ => Err(ExternalError::ProtocolDecode),
        }
    }

    pub(crate) async fn dispatch_event_result(
        &self,
        event: MessageEventKind,
        payload: MessageEvent,
    ) -> Result<(String, Vec<protocol::EventAction>), ExternalError> {
        let request_id = protocol::request_id();
        let frame = self.event(request_id.clone(), event, payload).await?;
        match frame {
            V6InboundFrame::EventResult { actions, .. } => Ok((request_id, actions)),
            V6InboundFrame::Error { message, .. } => Err(ExternalError::ModuleError(message)),
            _ => Err(ExternalError::ProtocolDecode),
        }
    }

    /// Push a companion-bot callback press to the module. Actions are not
    /// consumed here: the module responds through host.invoke methods.
    pub(crate) async fn dispatch_bot_callback(
        &self,
        payload: protocol::BotCallbackEvent,
    ) -> Result<(), ExternalError> {
        let request_id = protocol::request_id();
        let frame = self
            .request(
                V6OutboundCoreFrame::BotCallbackEvent {
                    request_id: request_id.clone(),
                    payload,
                },
                Expected::EventResult,
            )
            .await?;
        match frame {
            V6InboundFrame::EventResult { .. } => Ok(()),
            V6InboundFrame::Error { message, .. } => Err(ExternalError::ModuleError(message)),
            _ => Err(ExternalError::ProtocolDecode),
        }
    }

    pub(crate) async fn graceful_shutdown(&self) -> Result<(), ExternalError> {
        self.shutdown(protocol::request_id()).await
    }

    pub(crate) async fn terminate(&self) {
        let (reply, response) = oneshot::channel();
        if self
            .control
            .send(Control::ForceTerminate { reply })
            .await
            .is_ok()
        {
            let _ = response.await;
        }
    }

    pub(crate) async fn initialize(
        &self,
        request_id: String,
        module_id: String,
    ) -> Result<V6InboundFrame, ExternalError> {
        self.request(
            V6OutboundCoreFrame::Initialize {
                request_id,
                module_id,
            },
            Expected::Initialized,
        )
        .await
    }

    #[allow(dead_code)]
    pub(crate) async fn execute(
        &self,
        request_id: String,
        command: String,
        arguments: String,
        argument_entities: Vec<protocol::CustomEmojiEntity>,
    ) -> Result<V6InboundFrame, ExternalError> {
        self.execute_with_context(request_id, command, arguments, argument_entities, None)
            .await
    }

    pub(crate) async fn execute_with_context(
        &self,
        request_id: String,
        command: String,
        arguments: String,
        argument_entities: Vec<protocol::CustomEmojiEntity>,
        context: Option<protocol::V6CommandContext>,
    ) -> Result<V6InboundFrame, ExternalError> {
        self.request(
            V6OutboundCoreFrame::Execute {
                request_id,
                command,
                arguments,
                argument_entities,
                context,
            },
            Expected::Result,
        )
        .await
    }

    pub(crate) fn register_current_message(
        &self,
        message: grammers_client::message::Message,
        editable: bool,
    ) -> Result<String, ExternalError> {
        self.handles
            .lock()
            .map_err(|_| ExternalError::Unavailable)?
            .issue_message(message, editable)
            .map_err(|_| ExternalError::Unavailable)
    }
    pub(crate) fn register_reply_message(
        &self,
        message: grammers_client::message::Message,
    ) -> Result<String, ExternalError> {
        self.handles
            .lock()
            .map_err(|_| ExternalError::Unavailable)?
            .issue_reply_message(message)
            .map_err(|_| ExternalError::Unavailable)
    }
    pub(crate) fn release_handle(&self, handle: &str) {
        if self.host_active.load(Ordering::Acquire) != 0 {
            if let Ok(mut pending) = self.pending_handle_releases.lock()
                && !pending.iter().any(|item| item == handle)
            {
                pending.push(handle.to_owned());
            }
            return;
        }
        if let Ok(mut handles) = self.handles.lock() {
            handles.release(handle);
        }
    }
    pub(crate) fn register_peer_handle(
        &self,
        peer: grammers_session::types::PeerId,
    ) -> Result<String, ExternalError> {
        self.handles
            .lock()
            .map_err(|_| ExternalError::Unavailable)?
            .issue_peer(peer)
            .map_err(|_| ExternalError::Unavailable)
    }

    pub(crate) async fn event(
        &self,
        request_id: String,
        event: MessageEventKind,
        payload: MessageEvent,
    ) -> Result<V6InboundFrame, ExternalError> {
        self.request(
            V6OutboundCoreFrame::Event {
                request_id,
                event,
                payload,
            },
            Expected::EventResult,
        )
        .await
    }

    pub(crate) async fn health(&self, request_id: String) -> Result<V6InboundFrame, ExternalError> {
        self.request(V6OutboundCoreFrame::Health { request_id }, Expected::Health)
            .await
    }

    pub(crate) async fn shutdown(&self, request_id: String) -> Result<(), ExternalError> {
        let (reply, response) = oneshot::channel();
        self.control
            .send(Control::Shutdown { request_id, reply })
            .await
            .map_err(|_| ExternalError::Unavailable)?;
        response.await.map_err(|_| ExternalError::Unavailable)?
    }

    async fn request(
        &self,
        frame: V6OutboundCoreFrame,
        expected: Expected,
    ) -> Result<V6InboundFrame, ExternalError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.control
            .send(Control::Request {
                frame: Box::new(frame),
                expected,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ExternalError::Unavailable)?;
        reply_rx.await.map_err(|_| ExternalError::Unavailable)?
    }
}

enum Control {
    Request {
        frame: Box<V6OutboundCoreFrame>,
        expected: Expected,
        reply: oneshot::Sender<Result<V6InboundFrame, ExternalError>>,
    },
    Shutdown {
        request_id: String,
        reply: oneshot::Sender<Result<(), ExternalError>>,
    },
    ForceTerminate {
        reply: oneshot::Sender<()>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expected {
    Initialized,
    Health,
    Result,
    EventResult,
}

#[derive(Clone, Copy, Debug)]
enum FatalReason {
    Unavailable,
    ProtocolDecode,
    LineTooLarge,
    WrongRequestId,
    WrongModuleId,
    ExecutionTimeout,
    ShutdownTimeout,
    Backpressure,
    WriterUnavailable,
}

impl FatalReason {
    fn error(self) -> ExternalError {
        match self {
            Self::Unavailable => ExternalError::Unavailable,
            Self::ProtocolDecode => ExternalError::ProtocolDecode,
            Self::LineTooLarge => ExternalError::LineTooLarge,
            Self::WrongRequestId => ExternalError::WrongRequestId,
            Self::WrongModuleId => ExternalError::WrongModuleId,
            Self::ExecutionTimeout => ExternalError::ExecutionTimeout,
            Self::ShutdownTimeout => ExternalError::ShutdownTimeout,
            Self::Backpressure => ExternalError::Backpressure,
            Self::WriterUnavailable => ExternalError::WriterUnavailable,
        }
    }

    fn category(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::ProtocolDecode => "protocol_decode",
            Self::LineTooLarge => "line_too_large",
            Self::WrongRequestId => "wrong_request_id",
            Self::WrongModuleId => "wrong_module_id",
            Self::ExecutionTimeout => "execution_timeout",
            Self::ShutdownTimeout => "shutdown_timeout",
            Self::Backpressure => "backpressure",
            Self::WriterUnavailable => "writer_unavailable",
        }
    }

    fn from_writer(error: WriterQueueFailure) -> Self {
        match error {
            WriterQueueFailure::Backpressure => Self::Backpressure,
            WriterQueueFailure::Closed => Self::WriterUnavailable,
        }
    }

    fn from_inbound(error: &ExternalError) -> Self {
        match error {
            ExternalError::LineTooLarge => Self::LineTooLarge,
            _ => Self::ProtocolDecode,
        }
    }
}

struct Pending {
    request_id: String,
    expected: Expected,
    /// Deadline set only after the writer confirms the frame was written and
    /// flushed by the transport writer; it is absent while queued or awaiting
    /// flush.
    deadline: Option<Instant>,
    reply: oneshot::Sender<Result<V6InboundFrame, ExternalError>>,
}

/// A lifecycle request accepted by the supervisor but not yet sent to the
/// module, because the documented sequential contract allows at most one
/// in-flight lifecycle request at a time. Queued requests have no deadline;
/// their `V6_LIFECYCLE_TIMEOUT` begins after the dispatch flush acknowledgement.
struct QueuedRequest {
    frame: V6OutboundCoreFrame,
    expected: Expected,
    reply: oneshot::Sender<Result<V6InboundFrame, ExternalError>>,
}

#[derive(Debug)]
enum WriterCommand {
    Frame(V6OutboundCoreFrame, Flush),
}

#[derive(Debug)]
enum Flush {
    #[cfg(test)]
    None,
    Call(String),
    Lifecycle(String),
    Shutdown,
}

enum ActorEvent {
    Inbound(Result<V6InboundFrame, ExternalError>),
    RpcComplete {
        call_id: String,
        result: Result<serde_json::Value, V6CallError>,
    },
    HostComplete {
        call_id: String,
        result: Result<serde_json::Value, V6CallError>,
    },
    Flushed(String),
    LifecycleFlushed(String),
    ShutdownFlushed,
    WriterFailed,
    ReaderEof,
}

struct SupervisorIo {
    control_rx: mpsc::Receiver<Control>,
    reader_rx: mpsc::Receiver<ActorEvent>,
    writer_tx: mpsc::Sender<WriterCommand>,
    rpc_rx: mpsc::Receiver<ActorEvent>,
    actor_tx: mpsc::Sender<ActorEvent>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
    stderr_drain: JoinHandle<()>,
    stderr_capture: Arc<Mutex<StderrCapture>>,
}

#[allow(clippy::too_many_arguments)]
async fn supervise(
    mut child: Child,
    process_group: u32,
    descriptor: ExternalModuleDescriptor,
    executor: Arc<dyn V6TelegramExecutor>,
    runtime: Arc<Mutex<V6RuntimeState>>,
    handles: Arc<Mutex<V6HandleRegistry>>,
    host_active: Arc<AtomicUsize>,
    pending_handle_releases: Arc<Mutex<Vec<String>>>,
    host: Arc<V6HostExecutor>,
    io: SupervisorIo,
    restart_generation: u64,
) {
    let SupervisorIo {
        mut control_rx,
        mut reader_rx,
        writer_tx,
        mut rpc_rx,
        actor_tx,
        reader,
        writer,
        stderr_drain,
        stderr_capture,
    } = io;
    let mut in_flight: Option<Pending> = None;
    let mut waiting: VecDeque<QueuedRequest> = VecDeque::new();
    // A submitted lifecycle frame remains correlated until its writer task
    // reports one successful write+flush acknowledgement. This bookkeeping
    // survives response completion and shutdown cancellation.
    let mut awaiting_lifecycle_flush = HashSet::new();
    // Graceful shutdown cancels at most the one lifecycle request currently
    // dispatched. Keep its id boundedly so late frames for that request can be
    // drained without being mistaken for an unknown request.
    let mut cancelled_lifecycle_request = None;
    let mut active_calls = HashSet::new();
    let mut workers = JoinSet::new();
    let mut closing = false;
    let mut shutdown_reply: Option<oneshot::Sender<Result<(), ExternalError>>> = None;
    let mut force_reply: Option<oneshot::Sender<()>> = None;
    let mut shutdown_deadline = None;
    let mut shutdown_flushed = false;
    let mut stored_child_exit = None;
    let mut child_reaped = false;
    let mut reader_open = true;
    let mut control_open = true;
    let mut fatal_reason = None;
    let mut fatal_request_id = None;
    let mut fatal_stage = "runtime";
    let mut terminal_status = process::ProcessStatus::Crashed;
    let mut exit_code = None;
    let mut exit_signal = None;

    loop {
        tokio::select! {
            child_exit = child.wait(), if stored_child_exit.is_none() => {
                capture_exit_status(&child_exit, &mut exit_code, &mut exit_signal);
                child_reaped = child_exit.is_ok();
                if closing {
                    stored_child_exit = Some(child_exit);
                    if shutdown_flushed {
                        let result = shutdown_child_exit_result(true, stored_child_exit.take().expect("stored exit"));
                        let clean = result.is_ok();
                        if let Some(reply) = shutdown_reply.take() { let _ = reply.send(result); }
                        if clean {
                            terminal_status = process::ProcessStatus::Terminated;
                        } else {
                            fatal_reason = Some(FatalReason::Unavailable);
                            fatal_stage = "shutdown";
                        }
                        break;
                    }
                } else {
                    fatal_reason = Some(FatalReason::Unavailable);
                    fatal_stage = in_flight_stage(&in_flight);
                    break;
                }
            }
            control = control_rx.recv(), if control_open => match control {
                Some(Control::Request { frame, expected, reply }) => {
                    if closing { let _ = reply.send(Err(ExternalError::Unavailable)); continue; }
                    let request_stage = outbound_stage(&frame);
                    let Some(request_id) = request_id(&frame).map(str::to_owned) else { let _ = reply.send(Err(ExternalError::ProtocolEncode)); continue; };
                    if lifecycle_request_id_present(&in_flight, &waiting, &awaiting_lifecycle_flush, &request_id) {
                        let _ = reply.send(Err(ExternalError::WrongRequestId)); continue;
                    }
                    // Sequential contract: at most one lifecycle request is
                    // written to the module at a time. The rest wait in a
                    // bounded queue; their deadlines start only after dispatch
                    // has been written and flushed.
                    if in_flight.is_some() {
                        if waiting.len() == V6_MAX_PENDING {
                            let _ = reply.send(Err(ExternalError::Backpressure)); continue;
                        }
                        waiting.push_back(QueuedRequest { frame: *frame, expected, reply });
                        continue;
                    }
                    if let Err(reason) = dispatch_lifecycle(&mut in_flight, &mut awaiting_lifecycle_flush, &writer_tx, *frame, expected, reply) {
                        fatal_reason = Some(reason);
                        fatal_request_id = Some(request_id);
                        fatal_stage = request_stage;
                        break;
                    }
                }
                Some(Control::Shutdown { request_id, reply }) => {
                    if closing { let _ = reply.send(Err(ExternalError::Unavailable)); }
                    else if let Err(reason) = start_shutdown(&mut closing, &mut in_flight, &mut waiting, &mut workers, &writer_tx, request_id, Some(reply), &mut shutdown_reply, &mut cancelled_lifecycle_request) {
                        fatal_reason = Some(reason);
                        fatal_stage = "shutdown";
                        break;
                    }
                }
                Some(Control::ForceTerminate { reply }) => {
                    fail_lifecycle(&mut in_flight, &mut waiting);
                    workers.abort_all();
                    force_reply = Some(reply);
                    terminal_status = process::ProcessStatus::Terminated;
                    break;
                }
                None => {
                    control_open = false;
                    if !closing
                        && let Err(reason) = start_shutdown(&mut closing, &mut in_flight, &mut waiting, &mut workers, &writer_tx, "0".to_owned(), None, &mut shutdown_reply, &mut cancelled_lifecycle_request)
                    {
                        fatal_reason = Some(reason);
                        fatal_stage = "shutdown";
                        break;
                    }
                }
            },
            event = reader_rx.recv(), if reader_open => match event {
                Some(ActorEvent::Inbound(Ok(frame))) => match frame {
                    V6InboundFrame::TelegramInvoke(V6ModuleFrame::TelegramInvoke { call_id, method, params }) => {
                        if closing {
                            // The shutdown frame is a writer barrier: module
                            // work observed after it must not reserve a call or
                            // enqueue a result behind the barrier.
                            continue;
                        }
                        if !reserve_call_id(&mut active_calls, &call_id) {
                            fatal_reason = Some(FatalReason::ProtocolDecode);
                            fatal_stage = "rpc";
                            break;
                        }
                        host_active.fetch_add(1, Ordering::AcqRel);
                        if active_calls.len() > V6_MAX_ACTIVE_RPCS {
                            let error = V6CallError { kind: "capacity".to_owned(), message: "too many active calls".to_owned(), code: None, name: None, retry_after_seconds: None };
                            if let Err(queue_error) = queue_writer(&writer_tx, WriterCommand::Frame(V6OutboundCoreFrame::TelegramResult { call_id: call_id.clone(), result: Err(error) }, Flush::Call(call_id))) {
                                fatal_reason = Some(FatalReason::from_writer(queue_error));
                                fatal_stage = "rpc";
                                break;
                            }
                            continue;
                        }
                        match validate_invoke(&descriptor, &method, true) {
                            Ok(method) => {
                                let executor = executor.clone();
                                let module_id: Arc<str> = Arc::from(descriptor.id.as_str());
                                let tx = actor_tx.clone();
                                workers.spawn(async move {
                                    let result = match timeout(V6_RPC_TIMEOUT, executor.execute(V6ExecutionContext { module_id }, method, params)).await {
                                        Ok(result) => result,
                                        Err(_) => Err(V6ExecutorError::Timeout),
                                    };
                                    let _ = tx.send(ActorEvent::RpcComplete { call_id, result: map_executor_result(result) }).await;
                                });
                            }
                            Err(error) => {
                                if let Err(queue_error) = queue_writer(&writer_tx, WriterCommand::Frame(V6OutboundCoreFrame::TelegramResult { call_id: call_id.clone(), result: Err(error) }, Flush::Call(call_id))) {
                                    fatal_reason = Some(FatalReason::from_writer(queue_error));
                                    fatal_stage = "rpc";
                                    break;
                                }
                            }
                        }
                    }
                    V6InboundFrame::HostInvoke(V6ModuleFrame::HostInvoke { call_id, method, params }) => {
                        if closing {
                            // Shutdown is a writer barrier: never enqueue a
                            // host result behind it.
                            continue;
                        }
                        if active_calls.contains(&call_id) {
                            fatal_reason = Some(FatalReason::ProtocolDecode);
                            fatal_stage = "host";
                            break;
                        }
                        if !matches!(
                            method.as_str(),
                            "message.edit" | "message.sendBot" | "inline.form" | "inline.answer"
                                | "message.editBot" | "message.deleteInvoker"
                                | "message.deleteBot"
                        ) {
                            fatal_reason = Some(FatalReason::ProtocolDecode);
                            fatal_stage = "host";
                            break;
                        }
                        if active_calls.len() >= V6_MAX_ACTIVE_RPCS {
                            fatal_reason = Some(FatalReason::Backpressure);
                            fatal_stage = "host";
                            break;
                        }
                        if !reserve_call_id(&mut active_calls, &call_id) {
                            fatal_reason = Some(FatalReason::ProtocolDecode);
                            fatal_stage = "host";
                            break;
                        }
                        let host = host.clone();
                        let tx = actor_tx.clone();
                        workers.spawn(async move {
                            let result = match timeout(V6_RPC_TIMEOUT, host.execute(&method, params)).await {
                                Ok(Ok(value)) => Ok(value),
                                Ok(Err(message)) => Err(V6CallError { kind: "host".into(), message: message.into(), code: None, name: None, retry_after_seconds: None }),
                                Err(_) => Err(V6CallError { kind: "timeout".into(), message: "host call timed out".into(), code: None, name: None, retry_after_seconds: None }),
                            };
                            let _ = tx.send(ActorEvent::HostComplete { call_id, result }).await;
                        });
                    }
                    frame if terminal_request_id(&frame).is_some() => {
                        let Some(terminal_id) = terminal_request_id(&frame).map(str::to_owned) else {
                            fatal_reason = Some(FatalReason::ProtocolDecode);
                            break;
                        };
                        if closing
                            && cancelled_lifecycle_frame(&frame, &terminal_id, &cancelled_lifecycle_request)
                        {
                            continue;
                        }
                        let Some(pending_request) = take_in_flight(&mut in_flight, &terminal_id) else {
                            fatal_reason = Some(FatalReason::WrongRequestId);
                            fatal_request_id = Some(terminal_id);
                            fatal_stage = "runtime";
                            break;
                        };
                        let stage = expected_stage(pending_request.expected);
                        if !initialized_module_matches(&descriptor, &frame) {
                            let _ = pending_request.reply.send(Err(ExternalError::WrongModuleId));
                            fatal_reason = Some(FatalReason::WrongModuleId);
                            fatal_request_id = Some(terminal_id);
                            fatal_stage = stage;
                            break;
                        } else if expected_matches(pending_request.expected, &frame) {
                            let _ = pending_request.reply.send(Ok(frame));
                        } else {
                            let _ = pending_request.reply.send(Err(ExternalError::ProtocolDecode));
                            fatal_reason = Some(FatalReason::ProtocolDecode);
                            fatal_request_id = Some(terminal_id);
                            fatal_stage = stage;
                            break;
                        }
                        // The in-flight request completed; the next queued
                        // lifecycle request (if any) may now be dispatched. Its
                        // deadline begins after the dispatch flush acknowledgement.
                        if let Some(queued) = waiting.pop_front() {
                            let queued_id = request_id(&queued.frame).map(str::to_owned);
                            let queued_stage = outbound_stage(&queued.frame);
                            if let Err(reason) = dispatch_lifecycle(&mut in_flight, &mut awaiting_lifecycle_flush, &writer_tx, queued.frame, queued.expected, queued.reply) {
                                fatal_reason = Some(reason);
                                fatal_request_id = queued_id;
                                fatal_stage = queued_stage;
                                break;
                            }
                        }
                    }
                    V6InboundFrame::Log { request_id, level, message } => {
                        if closing
                            && cancelled_lifecycle_request.as_deref() == Some(request_id.as_str())
                        {
                            continue;
                        }
                        if in_flight
                            .as_ref()
                            .is_none_or(|request| request.request_id != request_id)
                        {
                            fatal_reason = Some(FatalReason::WrongRequestId);
                            fatal_request_id = Some(request_id);
                            fatal_stage = "runtime";
                            break;
                        }
                        process::log_module_message(&descriptor.id, &request_id, &level, &message);
                    }
                    _ => {
                        fatal_reason = Some(FatalReason::ProtocolDecode);
                        fatal_stage = in_flight_stage(&in_flight);
                        break;
                    },
                },
                Some(ActorEvent::Inbound(Err(error))) => {
                    fatal_reason = Some(FatalReason::from_inbound(&error));
                    fatal_request_id = in_flight_request_id(&in_flight);
                    fatal_stage = in_flight_stage(&in_flight);
                    break;
                },
                Some(ActorEvent::ReaderEof) | None => {
                    reader_open = false;
                    if reader_eof_is_fatal(closing) {
                        if let Some(reply) = shutdown_reply.take() { let _ = reply.send(Err(ExternalError::Unavailable)); }
                        fatal_reason = Some(FatalReason::Unavailable);
                        fatal_request_id = in_flight_request_id(&in_flight);
                        fatal_stage = in_flight_stage(&in_flight);
                        break;
                    }
                },
                _ => {}
            },
            event = rpc_rx.recv() => match event {
                Some(ActorEvent::RpcComplete { call_id, result }) => {
                    match handle_rpc_complete(closing, &mut active_calls, &writer_tx, call_id, result)
                    {
                        Ok(_) => {}
                        Err(reason) => {
                            fatal_reason = Some(reason);
                            fatal_stage = "rpc";
                            break;
                        }
                    }
                }
                Some(ActorEvent::HostComplete { call_id, result }) => {
                    if closing {
                        active_calls.remove(&call_id);
                        finish_host_call(&host_active, &pending_handle_releases, &handles);
                    } else if let Err(error) = queue_writer(
                        &writer_tx,
                        WriterCommand::Frame(
                            V6OutboundCoreFrame::HostResult { call_id: call_id.clone(), result },
                            Flush::Call(call_id),
                        ),
                    ) {
                        fatal_reason = Some(FatalReason::from_writer(error));
                        fatal_stage = "host";
                        break;
                    } else {
                        finish_host_call(&host_active, &pending_handle_releases, &handles);
                    }
                }
                Some(ActorEvent::Flushed(call_id)) => { active_calls.remove(&call_id); }
                Some(ActorEvent::LifecycleFlushed(request_id)) => {
                    if let Err(reason) = handle_lifecycle_flushed(
                        &mut in_flight,
                        &mut awaiting_lifecycle_flush,
                        &request_id,
                    ) {
                        fatal_reason = Some(reason);
                        fatal_request_id = Some(request_id);
                        fatal_stage = in_flight_stage(&in_flight);
                        break;
                    }
                }
                Some(ActorEvent::ShutdownFlushed) => {
                    shutdown_flushed = true;
                    shutdown_deadline = Some(Instant::now() + V6_SHUTDOWN_TIMEOUT);
                    if let Some(child_exit) = stored_child_exit.take() {
                        let result = shutdown_child_exit_result(true, child_exit);
                        let clean = result.is_ok();
                        if let Some(reply) = shutdown_reply.take() { let _ = reply.send(result); }
                        if clean {
                            terminal_status = process::ProcessStatus::Terminated;
                        } else {
                            fatal_reason = Some(FatalReason::Unavailable);
                            fatal_stage = "shutdown";
                        }
                        break;
                    }
                }
                Some(ActorEvent::WriterFailed) | None => {
                    if let Some(reply) = shutdown_reply.take() { let _ = reply.send(Err(ExternalError::WriterUnavailable)); }
                    fatal_reason = Some(FatalReason::WriterUnavailable);
                    fatal_request_id = in_flight_request_id(&in_flight);
                    fatal_stage = if closing { "shutdown" } else { in_flight_stage(&in_flight) };
                    break;
                },
                _ => {}
            },
            _ = async { if let Some(deadline) = shutdown_deadline { tokio::time::sleep_until(deadline).await } else { future::pending::<()>().await } }, if closing => {
                if let Some(reply) = shutdown_reply.take() { let _ = reply.send(Err(ExternalError::ShutdownTimeout)); }
                fatal_reason = Some(FatalReason::ShutdownTimeout);
                fatal_stage = "shutdown";
                break;
            },
            _ = sleep_until_in_flight_deadline(&in_flight), if lifecycle_deadline_started(&in_flight) => {
                if let Some(request) = in_flight.as_ref()
                    && request.deadline.is_some_and(|deadline| deadline <= Instant::now())
                {
                    let stage = expected_stage(request.expected);
                    let request_id = request.request_id.clone();
                    fatal_stage = stage;
                    fatal_reason = Some(FatalReason::ExecutionTimeout);
                    fatal_request_id = Some(request_id);
                    break;
                }
            },
        }
        while workers.try_join_next().is_some() {}
    }

    if fatal_request_id.is_none() {
        fatal_request_id = in_flight_request_id(&in_flight);
    }
    if let Some(reason) = fatal_reason {
        lock_runtime_state(&runtime).status = process::ProcessStatus::Crashed;
        fail_lifecycle_with(&mut in_flight, &mut waiting, reason);
    } else {
        fail_lifecycle(&mut in_flight, &mut waiting);
    }
    workers.abort_all();
    while workers.join_next().await.is_some() {}
    drop(writer_tx);
    reader.abort();
    writer.abort();

    kill_group(process_group);
    if !child_reaped {
        match timeout(V6_SHUTDOWN_TIMEOUT, child.wait()).await {
            Ok(result) => {
                capture_exit_status(&result, &mut exit_code, &mut exit_signal);
            }
            Err(_) => {
                let _ = child.kill().await;
                let result = child.wait().await;
                capture_exit_status(&result, &mut exit_code, &mut exit_signal);
            }
        }
    }

    process::finish_stderr_drain(stderr_drain).await;
    let _ = reader.await;
    let _ = writer.await;

    if let Some(reason) = fatal_reason {
        let capture = process::lock_capture(&stderr_capture).clone();
        let error = reason.error();
        let diagnostics = process::build_crash_diagnostics_with_context(
            &descriptor,
            fatal_request_id.as_deref(),
            &error,
            &capture,
            process::CrashDiagnosticContext {
                lifecycle_stage: fatal_stage,
                error_category: reason.category(),
                exit_code,
                signal: exit_signal,
                restart_generation,
            },
        );
        {
            let mut state = lock_runtime_state(&runtime);
            state.status = process::ProcessStatus::Crashed;
            state.diagnostic = Some(diagnostics.clone());
        }
        process::emit_crash_event(&diagnostics);
    } else {
        lock_runtime_state(&runtime).status = terminal_status;
    }
    if let Some(reply) = force_reply {
        let _ = reply.send(());
    }
    if let Ok(mut registry) = handles.lock() {
        *registry = V6HandleRegistry::with_generation(restart_generation + 1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriterQueueFailure {
    Backpressure,
    Closed,
}

fn queue_writer(
    writer: &mpsc::Sender<WriterCommand>,
    command: WriterCommand,
) -> Result<(), WriterQueueFailure> {
    match writer.try_send(command) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => Err(WriterQueueFailure::Backpressure),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(WriterQueueFailure::Closed),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcCompleteDisposition {
    /// The completion was written to the module's stdin.
    Written,
    /// The completion raced shutdown and was cancelled without a write.
    Discarded,
}

/// Shutdown barrier for RPC completions.
///
/// Once shutdown has begun, no newly processed RPC completion may enqueue a
/// `telegram.result` after the `shutdown` frame: the child may exit as soon as
/// it observes the shutdown frame, and a late write would race the closed pipe
/// into a false writer failure. A completion processed before shutdown may
/// still be written before shutdown. A completion processed after `closing`
/// becomes true is discarded cleanly and its `call_id` bookkeeping released,
/// without turning the race into a protocol crash.
fn handle_rpc_complete(
    closing: bool,
    active_calls: &mut HashSet<String>,
    writer: &mpsc::Sender<WriterCommand>,
    call_id: String,
    result: Result<serde_json::Value, V6CallError>,
) -> Result<RpcCompleteDisposition, FatalReason> {
    if closing {
        active_calls.remove(&call_id);
        return Ok(RpcCompleteDisposition::Discarded);
    }
    if let Err(error) = queue_writer(
        writer,
        WriterCommand::Frame(
            V6OutboundCoreFrame::TelegramResult {
                call_id: call_id.clone(),
                result,
            },
            Flush::Call(call_id),
        ),
    ) {
        return Err(FatalReason::from_writer(error));
    }
    Ok(RpcCompleteDisposition::Written)
}

fn reserve_call_id(active_calls: &mut HashSet<String>, call_id: &str) -> bool {
    active_calls.insert(call_id.to_owned())
}

fn finish_host_call(
    active: &AtomicUsize,
    pending: &Arc<Mutex<Vec<String>>>,
    handles: &Arc<Mutex<V6HandleRegistry>>,
) {
    if active.fetch_sub(1, Ordering::AcqRel) == 1
        && let Ok(mut pending) = pending.lock()
        && let Ok(mut handles) = handles.lock()
    {
        for handle in pending.drain(..) {
            handles.release(&handle);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_shutdown(
    closing: &mut bool,
    in_flight: &mut Option<Pending>,
    waiting: &mut VecDeque<QueuedRequest>,
    workers: &mut JoinSet<()>,
    writer: &mpsc::Sender<WriterCommand>,
    request_id: String,
    reply: Option<oneshot::Sender<Result<(), ExternalError>>>,
    shutdown_reply: &mut Option<oneshot::Sender<Result<(), ExternalError>>>,
    cancelled_lifecycle_request: &mut Option<String>,
) -> Result<(), FatalReason> {
    if *closing {
        return Ok(());
    }
    *closing = true;
    *cancelled_lifecycle_request = in_flight.as_ref().map(|request| request.request_id.clone());
    // Fail the in-flight request and every queued lifecycle request so no
    // reply or write is leaked behind the shutdown frame.
    fail_lifecycle(in_flight, waiting);
    workers.abort_all();
    if let Err(error) = queue_writer(
        writer,
        WriterCommand::Frame(
            V6OutboundCoreFrame::Shutdown { request_id },
            Flush::Shutdown,
        ),
    ) {
        let reason = FatalReason::from_writer(error);
        if let Some(reply) = reply {
            let _ = reply.send(Err(reason.error()));
        }
        return Err(reason);
    }
    *shutdown_reply = reply;
    Ok(())
}

fn cancelled_lifecycle_frame(
    frame: &V6InboundFrame,
    request_id: &str,
    cancelled_request: &Option<String>,
) -> bool {
    cancelled_request.as_deref() == Some(request_id)
        && terminal_request_id(frame) == Some(request_id)
}

fn expected_stage(expected: Expected) -> &'static str {
    match expected {
        Expected::Initialized => "initialize",
        Expected::Health => "health",
        Expected::Result => "execute",
        Expected::EventResult => "event",
    }
}

fn outbound_stage(frame: &V6OutboundCoreFrame) -> &'static str {
    match frame {
        V6OutboundCoreFrame::Initialize { .. } => "initialize",
        V6OutboundCoreFrame::Execute { .. } => "execute",
        V6OutboundCoreFrame::Event { .. } => "event",
        V6OutboundCoreFrame::BotCallbackEvent { .. } => "event",
        V6OutboundCoreFrame::Health { .. } => "health",
        V6OutboundCoreFrame::Shutdown { .. } => "shutdown",
        V6OutboundCoreFrame::TelegramResult { .. } => "rpc",
        V6OutboundCoreFrame::HostResult { .. } => "host",
    }
}

/// Submit a lifecycle request to the writer. The response deadline starts only
/// after the writer reports that transport write+flush completed.
fn dispatch_lifecycle(
    in_flight: &mut Option<Pending>,
    awaiting_lifecycle_flush: &mut HashSet<String>,
    writer: &mpsc::Sender<WriterCommand>,
    frame: V6OutboundCoreFrame,
    expected: Expected,
    reply: oneshot::Sender<Result<V6InboundFrame, ExternalError>>,
) -> Result<(), FatalReason> {
    let Some(request_id) = request_id(&frame).map(str::to_owned) else {
        let _ = reply.send(Err(ExternalError::ProtocolEncode));
        return Ok(());
    };
    if let Err(error) = queue_writer(
        writer,
        WriterCommand::Frame(frame, Flush::Lifecycle(request_id.clone())),
    ) {
        let reason = FatalReason::from_writer(error);
        let _ = reply.send(Err(reason.error()));
        return Err(reason);
    }
    awaiting_lifecycle_flush.insert(request_id.clone());
    *in_flight = Some(Pending {
        request_id,
        expected,
        deadline: None,
        reply,
    });
    Ok(())
}

/// True when the request id is currently in flight or already waiting in the
/// bounded lifecycle queue.
fn lifecycle_request_id_present(
    in_flight: &Option<Pending>,
    waiting: &VecDeque<QueuedRequest>,
    awaiting_lifecycle_flush: &HashSet<String>,
    query_id: &str,
) -> bool {
    in_flight
        .as_ref()
        .is_some_and(|request| request.request_id == query_id)
        || waiting
            .iter()
            .any(|queued| request_id(&queued.frame) == Some(query_id))
        || awaiting_lifecycle_flush.contains(query_id)
}

fn take_in_flight(in_flight: &mut Option<Pending>, request_id: &str) -> Option<Pending> {
    match in_flight.take() {
        Some(request) if request.request_id == request_id => Some(request),
        other => {
            *in_flight = other;
            None
        }
    }
}

fn in_flight_request_id(in_flight: &Option<Pending>) -> Option<String> {
    in_flight.as_ref().map(|request| request.request_id.clone())
}

fn in_flight_stage(in_flight: &Option<Pending>) -> &'static str {
    in_flight
        .as_ref()
        .map(|request| expected_stage(request.expected))
        .unwrap_or("runtime")
}

async fn sleep_until_in_flight_deadline(in_flight: &Option<Pending>) {
    match in_flight.as_ref().and_then(|request| request.deadline) {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => future::pending::<()>().await,
    }
}

fn lifecycle_deadline_started(in_flight: &Option<Pending>) -> bool {
    in_flight
        .as_ref()
        .is_some_and(|request| request.deadline.is_some())
}

fn handle_lifecycle_flushed(
    in_flight: &mut Option<Pending>,
    awaiting_lifecycle_flush: &mut HashSet<String>,
    request_id: &str,
) -> Result<(), FatalReason> {
    if !awaiting_lifecycle_flush.remove(request_id) {
        return Err(FatalReason::WrongRequestId);
    }
    if let Some(pending) = in_flight.as_mut()
        && pending.request_id == request_id
    {
        if pending.deadline.is_some() {
            return Err(FatalReason::WrongRequestId);
        }
        pending.deadline = Some(Instant::now() + lifecycle_timeout());
    }
    Ok(())
}

fn fail_lifecycle(in_flight: &mut Option<Pending>, waiting: &mut VecDeque<QueuedRequest>) {
    if let Some(request) = in_flight.take() {
        let _ = request.reply.send(Err(ExternalError::Unavailable));
    }
    for queued in waiting.drain(..) {
        let _ = queued.reply.send(Err(ExternalError::Unavailable));
    }
}

fn fail_lifecycle_with(
    in_flight: &mut Option<Pending>,
    waiting: &mut VecDeque<QueuedRequest>,
    reason: FatalReason,
) {
    if let Some(request) = in_flight.take() {
        let _ = request.reply.send(Err(reason.error()));
    }
    for queued in waiting.drain(..) {
        let _ = queued.reply.send(Err(reason.error()));
    }
}

fn capture_exit_status(
    result: &std::io::Result<std::process::ExitStatus>,
    exit_code: &mut Option<i32>,
    signal: &mut Option<i32>,
) {
    if let Ok(status) = result {
        *exit_code = status.code();
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            *signal = status.signal();
        }
    }
}

async fn read_stdout(
    mut stdout: BufReader<tokio::process::ChildStdout>,
    tx: mpsc::Sender<ActorEvent>,
    contract_revision: u32,
) {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        line.clear();
        loop {
            match stdout.read(&mut byte).await {
                Ok(0) => {
                    let _ = tx.send(ActorEvent::ReaderEof).await;
                    return;
                }
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) if line.len() >= protocol::MAX_LINE_BYTES => {
                    let _ = tx
                        .send(ActorEvent::Inbound(Err(ExternalError::LineTooLarge)))
                        .await;
                    return;
                }
                Ok(_) => line.push(byte[0]),
                Err(_) => {
                    let _ = tx
                        .send(ActorEvent::Inbound(Err(ExternalError::ProtocolDecode)))
                        .await;
                    return;
                }
            }
        }
        let parsed = std::str::from_utf8(&line)
            .map_err(|_| ExternalError::ProtocolDecode)
            .and_then(|line| protocol::parse_v6_inbound_frame_for(line, contract_revision));
        if tx.send(ActorEvent::Inbound(parsed)).await.is_err() {
            return;
        }
    }
}

async fn write_stdin(
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::Receiver<WriterCommand>,
    tx: mpsc::Sender<ActorEvent>,
) {
    while let Some(WriterCommand::Frame(frame, flush)) = rx.recv().await {
        let line = match frame.serialize() {
            Ok(line) => line,
            Err(_) => {
                let _ = tx.send(ActorEvent::WriterFailed).await;
                return;
            }
        };
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if !matches!(timeout(V6_WRITE_TIMEOUT, write).await, Ok(Ok(()))) {
            let _ = tx.send(ActorEvent::WriterFailed).await;
            return;
        }
        let event = match flush {
            #[cfg(test)]
            Flush::None => None,
            Flush::Call(call_id) => Some(ActorEvent::Flushed(call_id)),
            Flush::Lifecycle(request_id) => Some(ActorEvent::LifecycleFlushed(request_id)),
            Flush::Shutdown => Some(ActorEvent::ShutdownFlushed),
        };
        if let Some(event) = event
            && tx.send(event).await.is_err()
        {
            return;
        }
    }
}

fn validate_invoke(
    descriptor: &ExternalModuleDescriptor,
    method: &str,
    unique: bool,
) -> Result<v6_registry::V6Method, V6CallError> {
    if !unique {
        return Err(V6CallError {
            kind: "protocol".to_owned(),
            message: "duplicate call_id".to_owned(),
            code: None,
            name: None,
            retry_after_seconds: None,
        });
    }
    let method = v6_registry::lookup(method).ok_or_else(|| V6CallError {
        kind: "validation".to_owned(),
        message: "gateway method is not recognized".to_owned(),
        code: None,
        name: None,
        retry_after_seconds: None,
    })?;
    if !descriptor.telegram_methods.contains(&method) {
        return Err(V6CallError {
            kind: "capability".to_owned(),
            message: "method is not granted".to_owned(),
            code: None,
            name: None,
            retry_after_seconds: None,
        });
    }
    // Capability policy comes from the generated registry (`required_capability`)
    // — the same source manifest validation uses — so install-time and
    // runtime-time policy can never diverge for a future method.
    if let Some(required) = method.spec().required_capability
        && !descriptor
            .capabilities
            .iter()
            .any(|cap| cap.as_str() == required)
    {
        return Err(V6CallError {
            kind: "capability".to_owned(),
            message: format!("{required} capability is required"),
            code: None,
            name: None,
            retry_after_seconds: None,
        });
    }
    Ok(method)
}

fn map_executor_result(
    result: Result<super::v6_executor::V6RpcOutput, V6ExecutorError>,
) -> Result<serde_json::Value, V6CallError> {
    result
        .map(|value| value.into_value())
        .map_err(v6_executor_error)
}

fn v6_executor_error(error: V6ExecutorError) -> V6CallError {
    match error {
        V6ExecutorError::InvalidParams(_) => V6CallError {
            kind: "validation".to_owned(),
            message: "invalid parameters".to_owned(),
            code: None,
            name: None,
            retry_after_seconds: None,
        },
        V6ExecutorError::Rpc {
            code,
            name,
            retry_after_seconds,
        } => V6CallError {
            kind: "rpc".to_owned(),
            message: "Telegram RPC request failed".to_owned(),
            code: Some(code),
            name: Some(name),
            retry_after_seconds,
        },
        V6ExecutorError::Transport => V6CallError {
            kind: "transport".to_owned(),
            message: "Telegram transport unavailable".to_owned(),
            code: None,
            name: None,
            retry_after_seconds: None,
        },
        V6ExecutorError::Timeout => V6CallError {
            kind: "timeout".to_owned(),
            message: "Telegram RPC deadline exceeded".to_owned(),
            code: None,
            name: None,
            retry_after_seconds: None,
        },
        V6ExecutorError::InvalidResponse => V6CallError {
            kind: "internal".to_owned(),
            message: "invalid gateway response".to_owned(),
            code: None,
            name: None,
            retry_after_seconds: None,
        },
        V6ExecutorError::ShuttingDown => V6CallError {
            kind: "shutdown".to_owned(),
            message: "gateway is shutting down".to_owned(),
            code: None,
            name: None,
            retry_after_seconds: None,
        },
    }
}

fn request_id(frame: &V6OutboundCoreFrame) -> Option<&str> {
    match frame {
        V6OutboundCoreFrame::Initialize { request_id, .. }
        | V6OutboundCoreFrame::Execute { request_id, .. }
        | V6OutboundCoreFrame::Event { request_id, .. }
        | V6OutboundCoreFrame::BotCallbackEvent { request_id, .. }
        | V6OutboundCoreFrame::Health { request_id }
        | V6OutboundCoreFrame::Shutdown { request_id } => Some(request_id),
        V6OutboundCoreFrame::TelegramResult { .. } | V6OutboundCoreFrame::HostResult { .. } => None,
    }
}

fn terminal_request_id(frame: &V6InboundFrame) -> Option<&str> {
    match frame {
        V6InboundFrame::Initialized { request_id, .. }
        | V6InboundFrame::Result { request_id, .. }
        | V6InboundFrame::Error { request_id, .. }
        | V6InboundFrame::Health { request_id }
        | V6InboundFrame::EventResult { request_id, .. } => Some(request_id),
        V6InboundFrame::Log { .. }
        | V6InboundFrame::TelegramInvoke(_)
        | V6InboundFrame::HostInvoke(_) => None,
    }
}

fn expected_matches(expected: Expected, frame: &V6InboundFrame) -> bool {
    matches!(
        (expected, frame),
        (Expected::Initialized, V6InboundFrame::Initialized { .. })
            | (Expected::Health, V6InboundFrame::Health { .. })
            | (
                Expected::Result,
                V6InboundFrame::Result { .. } | V6InboundFrame::Error { .. }
            )
            | (
                Expected::EventResult,
                V6InboundFrame::EventResult { .. } | V6InboundFrame::Error { .. }
            )
    )
}

fn initialized_module_matches(
    descriptor: &ExternalModuleDescriptor,
    frame: &V6InboundFrame,
) -> bool {
    !matches!(frame, V6InboundFrame::Initialized { module_id, .. } if module_id != &descriptor.id)
}

fn shutdown_child_exit_result(
    flushed: bool,
    child_exit: std::io::Result<std::process::ExitStatus>,
) -> Result<(), ExternalError> {
    if !flushed {
        return Err(ExternalError::Unavailable);
    }
    match child_exit {
        Ok(status) if status.success() => Ok(()),
        Ok(_) | Err(_) => Err(ExternalError::Unavailable),
    }
}

fn reader_eof_is_fatal(closing: bool) -> bool {
    !closing
}

fn kill_group(process_group: u32) {
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-(process_group as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = process_group;
}

fn v6_module_state_dir<F>(module_id: &str, environment: &F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<OsString>,
{
    let base = environment("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            environment("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| Path::new(&home).join(".local/state"))
        })?;
    base.is_absolute()
        .then_some(base.join("lavis/modules").join(module_id))
}

#[cfg(test)]
static TEST_STATE_BASE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn set_test_state_base(base: PathBuf) {
    let _ = TEST_STATE_BASE.set(base);
}

#[cfg(test)]
pub(crate) fn ensure_test_state_base() {
    let base = std::env::temp_dir().join(format!("lavis-v6-test-state-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    set_test_state_base(base);
}

/// Resolve the module state directory. Tests install a writable base so the
/// Nix build sandbox (whose `$HOME` is not writable) can run the real-child
/// fixtures without leaking module state into the user's home.
fn resolve_v6_module_state_dir(module_id: &str) -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(base) = TEST_STATE_BASE.get() {
        return Some(base.join("lavis/modules").join(module_id));
    }
    v6_module_state_dir(module_id, &|name| std::env::var_os(name))
}

#[cfg(unix)]
async fn secure_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await
}

#[cfg(not(unix))]
async fn secure_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

async fn cleanup_spawned_child(child: &mut Child, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let ret = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        if ret == -1 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            let _ = child.kill().await;
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.wait().await;
}

#[cfg(test)]
#[path = "v6_process_tests.rs"]
mod tests;
