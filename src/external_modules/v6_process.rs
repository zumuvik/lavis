use super::{
    manifest::{ExternalCapability, ExternalModuleDescriptor},
    process::{self, StderrCapture},
    protocol::{
        self, MessageEvent, MessageEventKind, V6CallError, V6InboundFrame, V6ModuleFrame,
        V6OutboundCoreFrame,
    },
    v6_executor::{V6ExecutionContext, V6ExecutorError, V6TelegramExecutor},
    v6_registry,
};
use crate::error::ExternalError;
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    future,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
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

/// Cloneable front end to the single-owner V6 child-process actor.
#[derive(Clone)]
pub(crate) struct V6Process {
    control: mpsc::Sender<Control>,
    descriptor: Arc<ExternalModuleDescriptor>,
    runtime: Arc<Mutex<V6RuntimeState>>,
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
    pub(crate) async fn start(
        descriptor: ExternalModuleDescriptor,
        executor: Arc<dyn V6TelegramExecutor>,
        restart_generation: u64,
    ) -> Result<Self, ExternalError> {
        if descriptor.protocol_version != 6
            || !descriptor.entrypoint.starts_with(&descriptor.module_dir)
        {
            return Err(ExternalError::InvalidArgument);
        }

        let state_dir =
            resolve_v6_module_state_dir(&descriptor.id).ok_or(ExternalError::StateRead)?;
        tokio::fs::create_dir_all(&state_dir)
            .await
            .map_err(|_| ExternalError::StateWrite)?;
        secure_directory(&state_dir)
            .await
            .map_err(|_| ExternalError::StateWrite)?;
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
        let mut child = command.spawn().map_err(|_| ExternalError::Unavailable)?;
        let Some(process_group) = child.id() else {
            cleanup_spawned_child(&mut child, None).await;
            return Err(ExternalError::Unavailable);
        };
        let Some(stdin) = child.stdin.take() else {
            cleanup_spawned_child(&mut child, Some(process_group)).await;
            return Err(ExternalError::Unavailable);
        };
        let Some(stdout) = child.stdout.take() else {
            cleanup_spawned_child(&mut child, Some(process_group)).await;
            return Err(ExternalError::Unavailable);
        };
        let Some(stderr) = child.stderr.take() else {
            cleanup_spawned_child(&mut child, Some(process_group)).await;
            return Err(ExternalError::Unavailable);
        };

        let (control_tx, control_rx) = mpsc::channel(V6_CONTROL_QUEUE);
        let (reader_tx, reader_rx) = mpsc::channel(V6_READER_QUEUE);
        let (writer_tx, writer_rx) = mpsc::channel(V6_WRITER_QUEUE);
        let (rpc_tx, rpc_rx) = mpsc::channel(V6_RPC_QUEUE);
        let reader = tokio::spawn(read_stdout(BufReader::new(stdout), reader_tx));
        let writer = tokio::spawn(write_stdin(stdin, writer_rx, rpc_tx.clone()));
        let stderr_capture = Arc::new(Mutex::new(StderrCapture::default()));
        let stderr_drain = tokio::spawn(process::drain_stderr(stderr, stderr_capture.clone()));
        let runtime = Arc::new(Mutex::new(V6RuntimeState {
            status: process::ProcessStatus::Running,
            diagnostic: None,
        }));
        tokio::spawn(supervise(
            child,
            process_group,
            descriptor.clone(),
            executor,
            runtime.clone(),
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
            .execute(
                protocol::request_id(),
                command.to_owned(),
                arguments.to_owned(),
                argument_entities.to_vec(),
            )
            .await?;
        match frame {
            V6InboundFrame::Result { text, .. } => Ok(text),
            V6InboundFrame::Error { .. } => Err(ExternalError::ModuleError),
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
            V6InboundFrame::EventResult { actions, .. } => {
                let line = serde_json::json!({
                    "protocol_version": 6,
                    "type": "event_result",
                    "request_id": request_id,
                    "actions": actions,
                })
                .to_string();
                match protocol::parse_module_line_for(&line, 6)? {
                    Some(protocol::ModuleMessage::EventResult {
                        request_id,
                        actions,
                    }) => Ok((request_id, actions)),
                    _ => Err(ExternalError::ProtocolDecode),
                }
            }
            V6InboundFrame::Error { .. } => Err(ExternalError::ModuleError),
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

    pub(crate) async fn execute(
        &self,
        request_id: String,
        command: String,
        arguments: String,
        argument_entities: Vec<protocol::CustomEmojiEntity>,
    ) -> Result<V6InboundFrame, ExternalError> {
        self.request(
            V6OutboundCoreFrame::Execute {
                request_id,
                command,
                arguments,
                argument_entities,
            },
            Expected::Result,
        )
        .await
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
                frame,
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
        frame: V6OutboundCoreFrame,
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
    expected: Expected,
    deadline: Instant,
    reply: oneshot::Sender<Result<V6InboundFrame, ExternalError>>,
}

#[derive(Debug)]
enum WriterCommand {
    Frame(V6OutboundCoreFrame, Flush),
}

#[derive(Debug)]
enum Flush {
    None,
    Call(String),
    Shutdown,
}

enum ActorEvent {
    Inbound(Result<V6InboundFrame, ExternalError>),
    RpcComplete {
        call_id: String,
        result: Result<serde_json::Value, V6CallError>,
    },
    Flushed(String),
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

async fn supervise(
    mut child: Child,
    process_group: u32,
    descriptor: ExternalModuleDescriptor,
    executor: Arc<dyn V6TelegramExecutor>,
    runtime: Arc<Mutex<V6RuntimeState>>,
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
    let mut pending = HashMap::new();
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
                    fatal_stage = pending_stage(&pending);
                    break;
                }
            }
            control = control_rx.recv(), if control_open => match control {
                Some(Control::Request { frame, expected, reply }) => {
                    if closing { let _ = reply.send(Err(ExternalError::Unavailable)); continue; }
                    let request_stage = outbound_stage(&frame);
                    let Some(request_id) = request_id(&frame).map(str::to_owned) else { let _ = reply.send(Err(ExternalError::ProtocolEncode)); continue; };
                    if pending.contains_key(&request_id) { let _ = reply.send(Err(ExternalError::WrongRequestId)); continue; }
                    if pending.len() == V6_MAX_PENDING { let _ = reply.send(Err(ExternalError::Backpressure)); continue; }
                    if let Err(error) = queue_writer(&writer_tx, WriterCommand::Frame(frame, Flush::None)) {
                        let reason = FatalReason::from_writer(error);
                        let _ = reply.send(Err(reason.error()));
                        fatal_reason = Some(reason);
                        fatal_request_id = Some(request_id);
                        fatal_stage = request_stage;
                        break;
                    }
                    pending.insert(request_id, Pending { expected, deadline: Instant::now() + V6_LIFECYCLE_TIMEOUT, reply });
                }
                Some(Control::Shutdown { request_id, reply }) => {
                    if closing { let _ = reply.send(Err(ExternalError::Unavailable)); }
                    else if let Err(reason) = start_shutdown(&mut closing, &mut pending, &mut workers, &writer_tx, request_id, Some(reply), &mut shutdown_reply) {
                        fatal_reason = Some(reason);
                        fatal_stage = "shutdown";
                        break;
                    }
                }
                Some(Control::ForceTerminate { reply }) => {
                    fail_pending(&mut pending);
                    workers.abort_all();
                    force_reply = Some(reply);
                    terminal_status = process::ProcessStatus::Terminated;
                    break;
                }
                None => {
                    control_open = false;
                    if !closing
                        && let Err(reason) = start_shutdown(&mut closing, &mut pending, &mut workers, &writer_tx, "0".to_owned(), None, &mut shutdown_reply)
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
                        if !reserve_call_id(&mut active_calls, &call_id) {
                            fatal_reason = Some(FatalReason::ProtocolDecode);
                            fatal_stage = "rpc";
                            break;
                        }
                        if closing {
                            let rejected = V6CallError { kind: "shutdown".to_owned(), message: "module is shutting down".to_owned() };
                            if let Err(error) = queue_writer(&writer_tx, WriterCommand::Frame(V6OutboundCoreFrame::TelegramResult { call_id: call_id.clone(), result: Err(rejected) }, Flush::Call(call_id))) {
                                fatal_reason = Some(FatalReason::from_writer(error));
                                fatal_stage = "rpc";
                                break;
                            }
                            continue;
                        }
                        if active_calls.len() > V6_MAX_ACTIVE_RPCS {
                            let error = V6CallError { kind: "capacity".to_owned(), message: "too many active calls".to_owned() };
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
                    frame if terminal_request_id(&frame).is_some() => {
                        let Some(request_id) = terminal_request_id(&frame).map(str::to_owned) else {
                            fatal_reason = Some(FatalReason::ProtocolDecode);
                            break;
                        };
                        if let Some(pending_request) = pending.remove(&request_id) {
                            let stage = expected_stage(pending_request.expected);
                            if !initialized_module_matches(&descriptor, &frame) {
                                let _ = pending_request.reply.send(Err(ExternalError::WrongModuleId));
                                fatal_reason = Some(FatalReason::WrongModuleId);
                                fatal_request_id = Some(request_id);
                                fatal_stage = stage;
                                break;
                            } else if expected_matches(pending_request.expected, &frame) {
                                let _ = pending_request.reply.send(Ok(frame));
                            } else {
                                let _ = pending_request.reply.send(Err(ExternalError::ProtocolDecode));
                                fatal_reason = Some(FatalReason::ProtocolDecode);
                                fatal_request_id = Some(request_id);
                                fatal_stage = stage;
                                break;
                            }
                        } else {
                            fatal_reason = Some(FatalReason::WrongRequestId);
                            fatal_request_id = Some(request_id);
                            fatal_stage = "runtime";
                            break;
                        }
                    }
                    V6InboundFrame::Log { request_id, level, message } => {
                        if !pending.contains_key(&request_id) {
                            fatal_reason = Some(FatalReason::WrongRequestId);
                            fatal_request_id = Some(request_id);
                            fatal_stage = "runtime";
                            break;
                        }
                        process::log_module_message(&descriptor.id, &request_id, &level, &message);
                    }
                    _ => {
                        fatal_reason = Some(FatalReason::ProtocolDecode);
                        fatal_stage = pending_stage(&pending);
                        break;
                    },
                },
                Some(ActorEvent::Inbound(Err(error))) => {
                    fatal_reason = Some(FatalReason::from_inbound(&error));
                    fatal_request_id = single_pending_request_id(&pending);
                    fatal_stage = pending_stage(&pending);
                    break;
                },
                Some(ActorEvent::ReaderEof) | None => {
                    reader_open = false;
                    if reader_eof_is_fatal(closing) {
                        if let Some(reply) = shutdown_reply.take() { let _ = reply.send(Err(ExternalError::Unavailable)); }
                        fatal_reason = Some(FatalReason::Unavailable);
                        fatal_request_id = single_pending_request_id(&pending);
                        fatal_stage = pending_stage(&pending);
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
                Some(ActorEvent::Flushed(call_id)) => { active_calls.remove(&call_id); }
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
                    fatal_request_id = single_pending_request_id(&pending);
                    fatal_stage = if closing { "shutdown" } else { pending_stage(&pending) };
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
            _ = sleep_until_pending_deadline(&pending), if !pending.is_empty() => {
                if let Some(request_id) = expired_request_id(&pending) {
                    fatal_stage = pending.get(&request_id).map(|request| expected_stage(request.expected)).unwrap_or("runtime");
                    fatal_reason = Some(FatalReason::ExecutionTimeout);
                    fatal_request_id = Some(request_id);
                    break;
                }
            },
        }
        while workers.try_join_next().is_some() {}
    }

    if fatal_request_id.is_none() {
        fatal_request_id = single_pending_request_id(&pending);
    }
    if let Some(reason) = fatal_reason {
        lock_runtime_state(&runtime).status = process::ProcessStatus::Crashed;
        fail_pending_with(&mut pending, reason);
    } else {
        fail_pending(&mut pending);
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

fn start_shutdown(
    closing: &mut bool,
    pending: &mut HashMap<String, Pending>,
    workers: &mut JoinSet<()>,
    writer: &mpsc::Sender<WriterCommand>,
    request_id: String,
    reply: Option<oneshot::Sender<Result<(), ExternalError>>>,
    shutdown_reply: &mut Option<oneshot::Sender<Result<(), ExternalError>>>,
) -> Result<(), FatalReason> {
    if *closing {
        return Ok(());
    }
    *closing = true;
    fail_pending(pending);
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
        V6OutboundCoreFrame::Health { .. } => "health",
        V6OutboundCoreFrame::Shutdown { .. } => "shutdown",
        V6OutboundCoreFrame::TelegramResult { .. } => "rpc",
    }
}

fn pending_stage(pending: &HashMap<String, Pending>) -> &'static str {
    if pending.len() == 1 {
        pending
            .values()
            .next()
            .map(|request| expected_stage(request.expected))
            .unwrap_or("runtime")
    } else {
        "runtime"
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
            .and_then(protocol::parse_v6_inbound_frame);
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
            Flush::None => None,
            Flush::Call(call_id) => Some(ActorEvent::Flushed(call_id)),
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
        });
    }
    let method = v6_registry::lookup(method).ok_or_else(|| V6CallError {
        kind: "validation".to_owned(),
        message: "gateway method is not recognized".to_owned(),
    })?;
    if !descriptor.telegram_methods.contains(&method) {
        return Err(V6CallError {
            kind: "capability".to_owned(),
            message: "method is not granted".to_owned(),
        });
    }
    match method {
        v6_registry::V6Method::RawInvoke
            if !descriptor
                .capabilities
                .contains(&ExternalCapability::TelegramRaw) =>
        {
            return Err(V6CallError {
                kind: "capability".to_owned(),
                message: "telegram.raw capability is required".to_owned(),
            });
        }
        v6_registry::V6Method::AccountUpdateStatus
            if !descriptor
                .capabilities
                .contains(&ExternalCapability::TelegramAccountStatus) =>
        {
            return Err(V6CallError {
                kind: "capability".to_owned(),
                message: "telegram.account.status capability is required".to_owned(),
            });
        }
        _ => {}
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
        },
        V6ExecutorError::Rpc { .. } => V6CallError {
            kind: "rpc".to_owned(),
            message: "Telegram RPC request failed".to_owned(),
        },
        V6ExecutorError::Transport => V6CallError {
            kind: "transport".to_owned(),
            message: "Telegram transport unavailable".to_owned(),
        },
        V6ExecutorError::Timeout => V6CallError {
            kind: "timeout".to_owned(),
            message: "Telegram RPC deadline exceeded".to_owned(),
        },
        V6ExecutorError::InvalidResponse => V6CallError {
            kind: "internal".to_owned(),
            message: "invalid gateway response".to_owned(),
        },
        V6ExecutorError::ShuttingDown => V6CallError {
            kind: "shutdown".to_owned(),
            message: "gateway is shutting down".to_owned(),
        },
    }
}

fn request_id(frame: &V6OutboundCoreFrame) -> Option<&str> {
    match frame {
        V6OutboundCoreFrame::Initialize { request_id, .. }
        | V6OutboundCoreFrame::Execute { request_id, .. }
        | V6OutboundCoreFrame::Event { request_id, .. }
        | V6OutboundCoreFrame::Health { request_id }
        | V6OutboundCoreFrame::Shutdown { request_id } => Some(request_id),
        V6OutboundCoreFrame::TelegramResult { .. } => None,
    }
}

fn terminal_request_id(frame: &V6InboundFrame) -> Option<&str> {
    match frame {
        V6InboundFrame::Initialized { request_id, .. }
        | V6InboundFrame::Result { request_id, .. }
        | V6InboundFrame::Error { request_id, .. }
        | V6InboundFrame::Health { request_id }
        | V6InboundFrame::EventResult { request_id, .. } => Some(request_id),
        V6InboundFrame::Log { .. } | V6InboundFrame::TelegramInvoke(_) => None,
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

fn expired_request_id(pending: &HashMap<String, Pending>) -> Option<String> {
    let now = Instant::now();
    pending
        .iter()
        .find_map(|(request_id, request)| (request.deadline <= now).then(|| request_id.clone()))
}

fn single_pending_request_id(pending: &HashMap<String, Pending>) -> Option<String> {
    (pending.len() == 1)
        .then(|| pending.keys().next().cloned())
        .flatten()
}

async fn sleep_until_pending_deadline(pending: &HashMap<String, Pending>) {
    let deadline = pending
        .values()
        .map(|request| request.deadline)
        .min()
        .unwrap_or_else(Instant::now);
    tokio::time::sleep_until(deadline).await;
}

fn fail_pending(pending: &mut HashMap<String, Pending>) {
    for (_, request) in pending.drain() {
        let _ = request.reply.send(Err(ExternalError::Unavailable));
    }
}

fn fail_pending_with(pending: &mut HashMap<String, Pending>, reason: FatalReason) {
    for (_, request) in pending.drain() {
        let _ = request.reply.send(Err(reason.error()));
    }
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
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::external_modules::{
        manifest,
        source_inspection::{
            AcquiredLmod, InspectionConfig, InspectionLimits, ModuleInspector, OsRandom,
        },
        v6_executor::{self, V6ExecutorError},
    };

    fn descriptor() -> ExternalModuleDescriptor {
        ExternalModuleDescriptor {
            protocol_version: 6,
            id: "test".to_owned(),
            display_name: "Test".to_owned(),
            version: "1".to_owned(),
            author: "A".to_owned(),
            entrypoint: PathBuf::from("run"),
            module_dir: PathBuf::from("."),
            capabilities: vec![],
            default_command: None,
            subscriptions: vec![],
            telegram_methods: vec![],
            actions: vec![],
            commands: vec![],
        }
    }

    #[derive(Default)]
    struct RecordingExecutor {
        methods: std::sync::Mutex<Vec<v6_registry::V6Method>>,
    }

    impl V6TelegramExecutor for RecordingExecutor {
        fn execute<'a>(
            &'a self,
            _context: V6ExecutionContext,
            method: v6_registry::V6Method,
            _params: Box<serde_json::value::RawValue>,
        ) -> super::super::v6_executor::V6ExecutorFuture<'a> {
            self.methods.lock().unwrap().push(method);
            Box::pin(async {
                super::super::v6_executor::V6RpcOutput::new(serde_json::json!({"fixture": true}))
            })
        }
    }

    fn test_root(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lavis-{prefix}-{}-{}",
            std::process::id(),
            protocol::request_id()
        ))
    }

    /// Point the v6 module state directory at a writable temp base. The Nix
    /// build sandbox has a non-writable `$HOME`, so the real-child fixtures
    /// must not depend on the environment; every child-spawning test calls
    /// this before `V6Process::start`.
    fn ensure_test_state_base() {
        let base = std::env::temp_dir().join(format!("lavis-v6-test-state-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        set_test_state_base(base);
    }

    /// Absolute path to a `python3` interpreter found via the test process
    /// PATH, mirroring the legacy process fixtures. The child environment is
    /// cleared (`PATH=/usr/bin:/bin`), so fixture scripts must carry an
    /// absolute interpreter path that exists in the sandbox.
    fn python_executable() -> PathBuf {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join("python3"))
            .find(|candidate| candidate.is_file())
            .expect("fixture tests require python3 in PATH")
    }

    /// Write a v6 fixture script whose `#!/usr/bin/env python3` shebang is
    /// rewritten to the absolute interpreter path, so the cleared child
    /// environment can still execute it.
    #[cfg(unix)]
    fn write_v6_fixture(path: &Path, body: &str) {
        use std::{fs, os::unix::fs::PermissionsExt};
        let body = body.replacen(
            "#!/usr/bin/env python3",
            &format!("#!{}", python_executable().display()),
            1,
        );
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// In-memory variant of `write_v6_fixture` for archive payloads.
    #[cfg(unix)]
    fn fixture_script(body: &str) -> Vec<u8> {
        body.replacen(
            "#!/usr/bin/env python3",
            &format!("#!{}", python_executable().display()),
            1,
        )
        .into_bytes()
    }

    // Minimal STORED-only zip writer for packaged-module fixtures. Mirrors the
    // source_inspection test helper; no compression is used, so the archive is
    // inspectable by the production `inspect_into` path.
    const LOCAL: u32 = 0x0403_4b50;
    const CENTRAL: u32 = 0x0201_4b50;
    const EOCD: u32 = 0x0605_4b50;

    struct ArchiveEntry {
        name: String,
        data: Vec<u8>,
        mode: u32,
    }

    fn put16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn put32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn zip_entries(entries: &[ArchiveEntry]) -> Vec<u8> {
        let mut output = Vec::new();
        let mut offsets = Vec::new();
        for entry in entries {
            offsets.push(output.len() as u32);
            put32(&mut output, LOCAL);
            put16(&mut output, 20);
            put16(&mut output, 0);
            put16(&mut output, 0);
            put32(&mut output, 0);
            put32(&mut output, 0);
            put32(&mut output, entry.data.len() as u32);
            put32(&mut output, entry.data.len() as u32);
            put16(&mut output, entry.name.len() as u16);
            put16(&mut output, 0);
            output.extend_from_slice(entry.name.as_bytes());
            output.extend_from_slice(&entry.data);
        }
        let central_start = output.len() as u32;
        for (entry, offset) in entries.iter().zip(offsets) {
            put32(&mut output, CENTRAL);
            put16(&mut output, 0x0314);
            put16(&mut output, 20);
            put16(&mut output, 0);
            put16(&mut output, 0);
            put32(&mut output, 0);
            put32(&mut output, 0);
            put32(&mut output, entry.data.len() as u32);
            put32(&mut output, entry.data.len() as u32);
            put16(&mut output, entry.name.len() as u16);
            put16(&mut output, 0);
            put16(&mut output, 0);
            put16(&mut output, 0);
            put16(&mut output, 0);
            put32(&mut output, entry.mode << 16);
            put32(&mut output, offset);
            output.extend_from_slice(entry.name.as_bytes());
        }
        let central_length = output.len() as u32 - central_start;
        put32(&mut output, EOCD);
        put16(&mut output, 0);
        put16(&mut output, 0);
        put16(&mut output, entries.len() as u16);
        put16(&mut output, entries.len() as u16);
        put32(&mut output, central_length);
        put32(&mut output, central_start);
        put16(&mut output, 0);
        output
    }

    /// Opaque bodies received by a fake transport.
    type ReceivedCalls = Arc<std::sync::Mutex<Vec<(i32, Vec<u8>)>>>;

    /// Fake transport recording the opaque bodies it receives and answering
    /// with opaque bytes, so the packaged module's `raw.invoke` can be verified
    /// end-to-end without Telegram.
    struct FakeRawTlTransport {
        home_dc: Option<i32>,
        response: Vec<u8>,
        received: ReceivedCalls,
    }

    impl v6_executor::RawTlTransport for FakeRawTlTransport {
        fn home_dc_id(&self) -> Option<i32> {
            self.home_dc
        }

        fn invoke_in_dc(
            &self,
            dc_id: i32,
            body: Vec<u8>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<u8>, V6ExecutorError>> + Send + '_>,
        > {
            Box::pin(async move {
                self.received.lock().unwrap().push((dc_id, body));
                Ok(self.response.clone())
            })
        }
    }

    /// Executor routing `raw.invoke` through the transport-generic pipeline.
    struct PipelineExecutor {
        transport: Arc<dyn v6_executor::RawTlTransport>,
    }

    impl V6TelegramExecutor for PipelineExecutor {
        fn execute<'a>(
            &'a self,
            _context: V6ExecutionContext,
            method: v6_registry::V6Method,
            params: Box<serde_json::value::RawValue>,
        ) -> super::super::v6_executor::V6ExecutorFuture<'a> {
            let transport = self.transport.clone();
            Box::pin(async move {
                match method {
                    v6_registry::V6Method::RawInvoke => {
                        v6_executor::raw_invoke_pipeline(transport.as_ref(), params).await
                    }
                    _ => Err(V6ExecutorError::InvalidParams("unexpected method")),
                }
            })
        }
    }

    #[test]
    fn writer_queue_distinguishes_backpressure_from_closed_writer() {
        let (writer, mut receiver) = mpsc::channel(1);
        assert!(
            queue_writer(
                &writer,
                WriterCommand::Frame(
                    V6OutboundCoreFrame::Health {
                        request_id: "1".to_owned()
                    },
                    Flush::None,
                ),
            )
            .is_ok()
        );
        assert_eq!(
            queue_writer(
                &writer,
                WriterCommand::Frame(
                    V6OutboundCoreFrame::Health {
                        request_id: "2".to_owned()
                    },
                    Flush::None,
                ),
            ),
            Err(WriterQueueFailure::Backpressure),
        );
        assert!(receiver.try_recv().is_ok());
        drop(receiver);
        assert_eq!(
            queue_writer(
                &writer,
                WriterCommand::Frame(
                    V6OutboundCoreFrame::Health {
                        request_id: "3".to_owned()
                    },
                    Flush::None,
                ),
            ),
            Err(WriterQueueFailure::Closed),
        );
    }

    #[test]
    fn rpc_complete_before_shutdown_is_written() {
        let (writer, mut receiver) = mpsc::channel(1);
        let mut active_calls = HashSet::from(["call-1".to_owned()]);
        let disposition = handle_rpc_complete(
            false,
            &mut active_calls,
            &writer,
            "call-1".to_owned(),
            Ok(serde_json::json!({"fixture": true})),
        )
        .unwrap();
        assert_eq!(disposition, RpcCompleteDisposition::Written);
        assert!(active_calls.contains("call-1"));
        match receiver.try_recv().unwrap() {
            WriterCommand::Frame(
                V6OutboundCoreFrame::TelegramResult { call_id, .. },
                Flush::Call(flush_id),
            ) => {
                assert_eq!(call_id, "call-1");
                assert_eq!(flush_id, "call-1");
            }
            other => panic!("unexpected writer command: {other:?}"),
        }
    }

    #[test]
    fn rpc_complete_after_shutdown_is_discarded_without_write() {
        let (writer, mut receiver) = mpsc::channel(1);
        let mut active_calls = HashSet::from(["call-2".to_owned()]);
        let disposition = handle_rpc_complete(
            true,
            &mut active_calls,
            &writer,
            "call-2".to_owned(),
            Ok(serde_json::json!({"fixture": true})),
        )
        .unwrap();
        assert_eq!(disposition, RpcCompleteDisposition::Discarded);
        assert!(!active_calls.contains("call-2"));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn rpc_complete_releases_bookkeeping_for_unknown_call() {
        let (writer, mut receiver) = mpsc::channel(1);
        let mut active_calls = HashSet::from(["call-3".to_owned()]);
        let disposition = handle_rpc_complete(
            true,
            &mut active_calls,
            &writer,
            "unknown".to_owned(),
            Ok(serde_json::json!({})),
        )
        .unwrap();
        assert_eq!(disposition, RpcCompleteDisposition::Discarded);
        assert!(active_calls.contains("call-3"));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn rpc_complete_writer_backpressure_is_fatal() {
        let (writer, _receiver) = mpsc::channel(1);
        // Fill the queue so the completion hits backpressure.
        writer
            .try_send(WriterCommand::Frame(
                V6OutboundCoreFrame::Health {
                    request_id: "fill".to_owned(),
                },
                Flush::None,
            ))
            .unwrap();
        let mut active_calls = HashSet::new();
        let error = handle_rpc_complete(
            false,
            &mut active_calls,
            &writer,
            "call-4".to_owned(),
            Ok(serde_json::json!({})),
        )
        .unwrap_err();
        assert!(matches!(error, FatalReason::Backpressure));
    }

    /// Executor that records when the RPC future starts and only completes
    /// after a delay, so the completion reliably lands after shutdown begins.
    #[derive(Default)]
    struct DelayedExecutor {
        started: Arc<std::sync::atomic::AtomicBool>,
    }

    impl V6TelegramExecutor for DelayedExecutor {
        fn execute<'a>(
            &'a self,
            _context: V6ExecutionContext,
            _method: v6_registry::V6Method,
            _params: Box<serde_json::value::RawValue>,
        ) -> super::super::v6_executor::V6ExecutorFuture<'a> {
            let started = self.started.clone();
            Box::pin(async move {
                started.store(true, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                super::super::v6_executor::V6RpcOutput::new(serde_json::json!({"fixture": true}))
            })
        }
    }

    /// Regression: an RPC completing after shutdown began must not enqueue a
    /// `telegram.result` behind the `shutdown` frame. The child exits 5 if it
    /// ever observes a `telegram.result`; with the barrier it only receives the
    /// shutdown frame, exits 0, and shutdown completes cleanly.
    #[cfg(unix)]
    #[tokio::test]
    async fn rpc_completing_during_shutdown_is_discarded_and_shutdown_stays_clean() {
        use std::fs;
        let root = test_root("v6-rpc-shutdown-barrier");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let entrypoint = root.join("run");
        ensure_test_state_base();

        let mut module = descriptor();
        module.id = format!("v6rpcshutdown{}", std::process::id());
        module.module_dir = root.clone();
        module.entrypoint = entrypoint.clone();
        module.telegram_methods = vec![v6_registry::V6Method::ContactsGetContacts];

        let script = r#"#!/usr/bin/env python3
import sys, json

def req_id(line):
    return json.loads(line)["request_id"]

line = sys.stdin.readline()
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "initialized", "request_id": rid, "module_id": "__MODULE_ID__"}), flush=True)
line = sys.stdin.readline()
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "health", "request_id": rid}), flush=True)
print('{"protocol_version":6,"type":"telegram.invoke","call_id":"race-1","method":"contacts.getContacts","params":{"hash":"0"}}', flush=True)
line = sys.stdin.readline()
if line and '"telegram.result"' in line:
    print('telegram.result delivered after shutdown', file=sys.stderr)
    sys.stderr.flush()
    sys.exit(5)
sys.exit(0)
"#
        .replace("__MODULE_ID__", &module.id);
        write_v6_fixture(&entrypoint, &script);

        let executor = Arc::new(DelayedExecutor::default());
        let v6proc = V6Process::start(module.clone(), executor.clone(), 1)
            .await
            .unwrap();
        v6proc.initialize("1".to_owned(), module.id).await.unwrap();
        v6proc.health("2".to_owned()).await.unwrap();

        // Wait until the RPC is in flight, then begin shutdown so the
        // completion lands while the supervisor is closing.
        for _ in 0..100 {
            if executor.started.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(executor.started.load(std::sync::atomic::Ordering::SeqCst));

        assert!(v6proc.graceful_shutdown().await.is_ok());
        assert_eq!(v6proc.status(), process::ProcessStatus::Terminated);
        assert!(v6proc.diagnostic().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// Packaged `.lmod` conformance: the archive is inspected and validated
    /// through the production pipeline (zip inspection + manifest validation),
    /// then the v6 child is started and drives `raw.invoke` with opaque TL
    /// bytes for `messages.sendMessage` — a valid method with no typed registry
    /// entry — through a fake transport and back into the module.
    #[cfg(unix)]
    #[tokio::test]
    async fn packaged_lmod_conforms_through_raw_invoke_with_opaque_bytes() {
        use std::fs;
        let root = test_root("v6-packaged");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let module_id = format!("packagedv6{}", std::process::id());
        let body = vec![0x70, 0x38, 0x0c, 0x52, 0x01, 0x00, 0x00, 0x00]; // messages.sendMessage
        assert!(
            v6_registry::lookup("messages.sendMessage").is_none(),
            "fixture method must not be in the typed registry"
        );
        let body_base64 = v6_executor::encode_base64(&body);
        let marker = "marker.txt";
        let script = r#"#!/usr/bin/env python3
import sys, json, base64, os

def write_marker(content):
    # Atomic visibility: the marker path only appears with full content, so a
    # poller can never observe a partially-buffered write.
    tmp = "__MARKER__.tmp"
    with open(tmp, "w") as f:
        f.write(content)
    os.replace(tmp, "__MARKER__")

def req_id(line):
    return json.loads(line)["request_id"]

line = sys.stdin.readline()
if not line:
    sys.exit(60)
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "initialized", "request_id": rid, "module_id": "__MODULE_ID__"}), flush=True)
line = sys.stdin.readline()
if not line:
    sys.exit(61)
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "health", "request_id": rid}), flush=True)
print('{"protocol_version":6,"type":"telegram.invoke","call_id":"pkg-1","method":"raw.invoke","params":{"dc_id":2,"body_base64_chunks":["__BODY_BASE64__"]}}', flush=True)
line = sys.stdin.readline()
try:
    frame = json.loads(line)
    assert frame["type"] == "telegram.result", frame.get("type")
    result = frame["result"]
    assert result["kind"] == "raw_tl", result.get("kind")
    assert result["dc_id"] == 2, result.get("dc_id")
    decoded = base64.b64decode("".join(result["body_base64_chunks"]))
    write_marker(decoded.decode("latin-1"))
except Exception as error:
    write_marker("FAILED: " + repr(error))
    sys.exit(62)
line = sys.stdin.readline()
sys.exit(0)
"#
        .replace("__MODULE_ID__", &module_id)
        .replace("__BODY_BASE64__", &body_base64)
        .replace("__MARKER__", marker);

        let manifest_json = format!(
            r#"{{"schema_version":6,"id":"{module_id}","name":"Packaged","version":"1","author":"A","entrypoint":"run","capabilities":["telegram.raw"],"telegram_methods":["raw.invoke"],"commands":[{{"name":"go","summary_ru":"x","description_ru":"x","usage":"<value>"}}]}}"#
        );
        let archive = zip_entries(&[
            ArchiveEntry {
                name: "module.json".to_owned(),
                data: manifest_json.into_bytes(),
                mode: 0o100644,
            },
            ArchiveEntry {
                name: "run".to_owned(),
                data: fixture_script(&script),
                mode: 0o100755,
            },
        ]);

        // Production inspection pipeline: staging, zip inspection, manifest
        // validation, then the validated descriptor.
        let config = InspectionConfig {
            staging_root: root.clone(),
            limits: InspectionLimits::default(),
        };
        let mut inspector = ModuleInspector::new(&config, OsRandom);
        let pending = inspector
            .inspect(
                AcquiredLmod::archive(archive),
                std::time::SystemTime::now(),
                std::time::SystemTime::now(),
            )
            .expect("packaged module must pass inspection");
        let payload = pending.stage.take_wrapper().unwrap().join("payload");
        let descriptor =
            manifest::validate_manifest_at(&payload.join("module.json"), Some(&module_id))
                .expect("production manifest validation");
        assert_eq!(descriptor.protocol_version, 6);
        assert_eq!(
            descriptor.telegram_methods,
            vec![v6_registry::V6Method::RawInvoke]
        );

        ensure_test_state_base();
        let transport = FakeRawTlTransport {
            home_dc: Some(2),
            response: b"opaque-response-ok".to_vec(),
            received: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let received = transport.received.clone();
        let executor = Arc::new(PipelineExecutor {
            transport: Arc::new(transport),
        });
        let v6proc = V6Process::start(descriptor, executor, 1).await.unwrap();
        v6proc
            .initialize("1".to_owned(), module_id.clone())
            .await
            .unwrap();
        v6proc.health("2".to_owned()).await.unwrap();

        let marker_path = payload.join(marker);
        let mut verified = false;
        for _ in 0..300 {
            if marker_path.exists() {
                verified = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(verified, "module must receive the raw response");
        let marker_text = fs::read_to_string(&marker_path).unwrap();
        assert!(
            !marker_text.starts_with("FAILED"),
            "module rejected the response: {marker_text}"
        );
        assert_eq!(marker_text, "opaque-response-ok");

        // Both directions verified with opaque bytes: the exact module body
        // reached the transport, and the exact transport response reached the
        // module.
        {
            let received = received.lock().unwrap();
            assert_eq!(received.len(), 1);
            assert_eq!(received[0].0, 2);
            assert_eq!(received[0].1, body);
        }

        assert!(v6proc.graceful_shutdown().await.is_ok());
        assert_eq!(v6proc.status(), process::ProcessStatus::Terminated);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_shutdown_is_crash_and_retains_exit_and_stderr() {
        use std::fs;
        let root = test_root("v6-bad-shutdown");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let entrypoint = root.join("run");
        ensure_test_state_base();
        write_v6_fixture(
            &entrypoint,
            "#!/usr/bin/env python3\nimport sys, os\nsys.stdin.readline()\nprint('shutdown-broke', file=sys.stderr)\nsys.stderr.flush()\nsys.exit(7)\n",
        );

        let mut module = descriptor();
        module.id = format!("v6badshutdown{}", std::process::id());
        module.module_dir = root.clone();
        module.entrypoint = entrypoint;
        let v6proc = V6Process::start(module, Arc::new(RecordingExecutor::default()), 1)
            .await
            .unwrap();

        assert!(matches!(
            v6proc.graceful_shutdown().await,
            Err(ExternalError::Unavailable)
        ));
        for _ in 0..200 {
            if v6proc.status() == process::ProcessStatus::Crashed && v6proc.diagnostic().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(v6proc.status(), process::ProcessStatus::Crashed);
        let diagnostic = v6proc.diagnostic().expect("crash diagnostic");
        assert_eq!(diagnostic.lifecycle_stage, "shutdown");
        assert_eq!(diagnostic.exit_code, Some(7));
        assert_eq!(diagnostic.error_category, "unavailable");
        assert!(diagnostic.stderr.contains("shutdown-broke"));
        assert!(diagnostic.timestamp_unix_ms > 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn force_terminate_kills_process_and_reports_terminated() {
        use std::fs;
        let root = test_root("v6-force");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let entrypoint = root.join("run");
        ensure_test_state_base();
        write_v6_fixture(
            &entrypoint,
            "#!/usr/bin/env python3\nimport os, time\nwith open('leader.pid', 'w') as f:\n    f.write(str(os.getpid()))\ntime.sleep(30)\n",
        );

        let mut module = descriptor();
        module.id = format!("v6force{}", std::process::id());
        module.module_dir = root.clone();
        module.entrypoint = entrypoint;
        let v6proc = V6Process::start(module, Arc::new(RecordingExecutor::default()), 1)
            .await
            .unwrap();
        let pid_path = root.join("leader.pid");
        for _ in 0..100 {
            if pid_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let leader: i32 = fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        v6proc.terminate().await;
        assert_eq!(v6proc.status(), process::ProcessStatus::Terminated);
        assert!(v6proc.diagnostic().is_none());
        let proc_stat = fs::read_to_string(format!("/proc/{leader}/stat"));
        let live = match proc_stat {
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_none_or(|(_, tail)| !tail.starts_with('Z')),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => true,
        };
        let _ = fs::remove_dir_all(&root);
        assert!(!live, "force-terminated module leader remained alive");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_child_frame_crashes_with_retained_protocol_diagnostic() {
        use std::fs;
        let root = test_root("v6-malformed");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let entrypoint = root.join("run");
        ensure_test_state_base();
        write_v6_fixture(
            &entrypoint,
            "#!/usr/bin/env python3\nimport sys, time\nsys.stdin.readline()\nprint('malformed-frame', file=sys.stderr)\nsys.stderr.flush()\nprint('{broken-json')\nsys.stdout.flush()\ntime.sleep(30)\n",
        );

        let mut module = descriptor();
        module.id = format!("v6malformed{}", std::process::id());
        module.module_dir = root.clone();
        module.entrypoint = entrypoint;
        let v6proc = V6Process::start(module.clone(), Arc::new(RecordingExecutor::default()), 1)
            .await
            .unwrap();
        assert!(matches!(
            v6proc.initialize("1".to_owned(), module.id).await,
            Err(ExternalError::ProtocolDecode)
        ));
        for _ in 0..200 {
            if v6proc.diagnostic().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let diagnostic = v6proc.diagnostic().expect("protocol diagnostic");
        assert_eq!(diagnostic.lifecycle_stage, "initialize");
        assert_eq!(diagnostic.error_category, "protocol_decode");
        assert!(diagnostic.stderr.contains("malformed-frame"));
        assert_eq!(v6proc.status(), process::ProcessStatus::Crashed);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lifecycle_timeout_is_fatal_and_retained() {
        use std::fs;
        let root = test_root("v6-timeout");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let entrypoint = root.join("run");
        ensure_test_state_base();
        write_v6_fixture(
            &entrypoint,
            "#!/usr/bin/env python3\nimport sys, time\nsys.stdin.readline()\ntime.sleep(30)\n",
        );

        let mut module = descriptor();
        module.id = format!("v6timeout{}", std::process::id());
        module.module_dir = root.clone();
        module.entrypoint = entrypoint;
        let v6proc = V6Process::start(module.clone(), Arc::new(RecordingExecutor::default()), 1)
            .await
            .unwrap();
        assert!(matches!(
            v6proc.initialize("1".to_owned(), module.id).await,
            Err(ExternalError::ExecutionTimeout)
        ));
        for _ in 0..200 {
            if v6proc.diagnostic().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let diagnostic = v6proc.diagnostic().expect("timeout diagnostic");
        assert_eq!(diagnostic.lifecycle_stage, "initialize");
        assert_eq!(diagnostic.error_category, "execution_timeout");
        assert_eq!(v6proc.status(), process::ProcessStatus::Crashed);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_child_conformance_covers_lifecycle_curated_and_raw_rpc() {
        use std::fs;
        let root = test_root("v6-conformance");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let entrypoint = root.join("run");

        let mut module = descriptor();
        module.id = format!("v6conformance{}", std::process::id());
        module.module_dir = root.clone();
        module.entrypoint = entrypoint.clone();
        module.telegram_methods = vec![
            v6_registry::V6Method::ContactsGetContacts,
            v6_registry::V6Method::RawInvoke,
        ];
        module.capabilities.push(ExternalCapability::TelegramRaw);

        ensure_test_state_base();
        let script = r#"#!/usr/bin/env python3
import sys, json

def req_id(line):
    return json.loads(line)["request_id"]

line = sys.stdin.readline()
if not line:
    sys.exit(80)
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "initialized", "request_id": rid, "module_id": "__MODULE_ID__"}), flush=True)
line = sys.stdin.readline()
if not line:
    sys.exit(81)
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "result", "request_id": rid, "text": "ok"}), flush=True)
line = sys.stdin.readline()
if not line:
    sys.exit(82)
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "event_result", "request_id": rid, "actions": []}), flush=True)
print('{"protocol_version":6,"type":"telegram.invoke","call_id":"curated-1","method":"contacts.getContacts","params":{"hash":"0"}}', flush=True)
line = sys.stdin.readline()
if not line:
    sys.exit(83)
print('{"protocol_version":6,"type":"telegram.invoke","call_id":"raw-1","method":"raw.invoke","params":{"body_base64_chunks":["eFY0EgEAAAA="]}}', flush=True)
line = sys.stdin.readline()
if not line:
    sys.exit(84)
line = sys.stdin.readline()
if not line:
    sys.exit(85)
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "health", "request_id": rid}), flush=True)
line = sys.stdin.readline()
if not line:
    sys.exit(86)
sys.exit(0)
"#
        .replace("__MODULE_ID__", &module.id);
        write_v6_fixture(&entrypoint, &script);

        let executor = Arc::new(RecordingExecutor::default());
        let v6proc = V6Process::start(module.clone(), executor.clone(), 1)
            .await
            .unwrap();
        assert!(matches!(
            v6proc
                .initialize("1".to_owned(), module.id.clone())
                .await
                .unwrap(),
            V6InboundFrame::Initialized { .. }
        ));
        assert!(matches!(
            v6proc.execute("2".to_owned(), "run".to_owned(), String::new(), vec![]).await.unwrap(),
            V6InboundFrame::Result { text, .. } if text == "ok"
        ));
        assert!(matches!(
            v6proc
                .event(
                    "3".to_owned(),
                    MessageEventKind::Created,
                    MessageEvent {
                        event_id: "e".to_owned(),
                        message_ref: "r".to_owned(),
                        message_key: "k".to_owned(),
                        peer_id: None,
                        text: "hello".to_owned(),
                        outgoing: false,
                        entities: vec![],
                    },
                )
                .await
                .unwrap(),
            V6InboundFrame::EventResult { .. }
        ));
        for _ in 0..200 {
            if executor.methods.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            *executor.methods.lock().unwrap(),
            vec![
                v6_registry::V6Method::ContactsGetContacts,
                v6_registry::V6Method::RawInvoke
            ]
        );
        assert!(matches!(
            v6proc.health("4".to_owned()).await.unwrap(),
            V6InboundFrame::Health { .. }
        ));
        v6proc.graceful_shutdown().await.unwrap();
        for _ in 0..100 {
            if v6proc.status() == process::ProcessStatus::Terminated {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(v6proc.status(), process::ProcessStatus::Terminated);
        assert!(v6proc.diagnostic().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ungranted_method_is_rejected_before_executor() {
        let descriptor = descriptor();
        assert!(
            matches!(validate_invoke(&descriptor, "account.updateStatus", true), Err(V6CallError { kind, .. }) if kind == "capability")
        );
    }

    #[test]
    fn curated_methods_do_not_require_raw_but_raw_does() {
        let mut descriptor = descriptor();
        descriptor.telegram_methods = vec![v6_registry::V6Method::ContactsGetContacts];
        assert_eq!(
            validate_invoke(&descriptor, "contacts.getContacts", true).unwrap(),
            v6_registry::V6Method::ContactsGetContacts
        );

        descriptor.telegram_methods = vec![v6_registry::V6Method::RawInvoke];
        assert!(matches!(
            validate_invoke(&descriptor, "raw.invoke", true),
            Err(V6CallError { kind, .. }) if kind == "capability"
        ));
        descriptor
            .capabilities
            .push(ExternalCapability::TelegramRaw);
        assert_eq!(
            validate_invoke(&descriptor, "raw.invoke", true).unwrap(),
            v6_registry::V6Method::RawInvoke
        );
    }

    #[test]
    fn terminal_frames_preserve_their_request_id() {
        let frame = V6InboundFrame::Health {
            request_id: "12".to_owned(),
        };
        assert_eq!(terminal_request_id(&frame), Some("12"));
        assert!(expected_matches(Expected::Health, &frame));
    }

    #[test]
    fn initialized_module_id_must_match_descriptor() {
        let frame = V6InboundFrame::Initialized {
            request_id: "12".to_owned(),
            module_id: "other".to_owned(),
        };
        assert!(!initialized_module_matches(&descriptor(), &frame));
    }

    #[test]
    fn executor_errors_are_sanitized() {
        assert_eq!(
            v6_executor_error(V6ExecutorError::InvalidParams("secret")),
            V6CallError {
                kind: "validation".to_owned(),
                message: "invalid parameters".to_owned()
            }
        );
        assert_eq!(v6_executor_error(V6ExecutorError::Timeout).kind, "timeout");
        assert_eq!(
            v6_executor_error(V6ExecutorError::Rpc {
                code: 420,
                name: "FLOOD_WAIT".to_owned(),
                retry_after_seconds: Some(7)
            })
            .kind,
            "rpc"
        );
    }

    #[test]
    fn shutdown_requires_a_flushed_shutdown_frame_before_child_exit() {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            let exited = Ok(std::process::ExitStatus::from_raw(0));
            assert!(matches!(
                shutdown_child_exit_result(false, exited),
                Err(ExternalError::Unavailable)
            ));
            assert!(
                shutdown_child_exit_result(true, Ok(std::process::ExitStatus::from_raw(0))).is_ok()
            );
            assert!(matches!(
                shutdown_child_exit_result(true, Ok(std::process::ExitStatus::from_raw(1 << 8))),
                Err(ExternalError::Unavailable)
            ));
        }
        assert!(!reader_eof_is_fatal(true));
        assert!(reader_eof_is_fatal(false));
    }

    #[test]
    fn call_ids_remain_unique_while_closing() {
        let mut active = HashSet::new();
        assert!(reserve_call_id(&mut active, "call"));
        assert!(!reserve_call_id(&mut active, "call"));
    }

    #[test]
    fn pending_expiry_drains_expired_requests() {
        let mut pending = HashMap::new();
        let (reply, mut receiver) = oneshot::channel();
        pending.insert(
            "1".to_owned(),
            Pending {
                expected: Expected::Health,
                deadline: Instant::now() - Duration::from_secs(1),
                reply,
            },
        );
        assert_eq!(expired_request_id(&pending), Some("1".to_owned()));
        fail_pending_with(&mut pending, FatalReason::ExecutionTimeout);
        assert!(pending.is_empty());
        assert!(matches!(
            receiver.try_recv(),
            Ok(Err(ExternalError::ExecutionTimeout))
        ));
    }

    #[tokio::test]
    async fn pending_deadline_is_absolute_under_event_activity() {
        let mut pending = HashMap::new();
        let (reply, _receiver) = oneshot::channel();
        pending.insert(
            "1".to_owned(),
            Pending {
                expected: Expected::Health,
                deadline: Instant::now(),
                reply,
            },
        );
        tokio::time::timeout(
            Duration::from_millis(20),
            sleep_until_pending_deadline(&pending),
        )
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_kills_descendant_after_leader_exit() {
        use std::fs;

        struct NoopExecutor;
        impl V6TelegramExecutor for NoopExecutor {
            fn execute<'a>(
                &'a self,
                _context: V6ExecutionContext,
                _method: v6_registry::V6Method,
                _params: Box<serde_json::value::RawValue>,
            ) -> super::super::v6_executor::V6ExecutorFuture<'a> {
                Box::pin(async { Err(V6ExecutorError::Transport) })
            }
        }

        let root = std::env::temp_dir().join(format!(
            "lavis-v6-descendant-{}-{}",
            std::process::id(),
            protocol::request_id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let entrypoint = root.join("run");
        ensure_test_state_base();
        write_v6_fixture(
            &entrypoint,
            "#!/usr/bin/env python3\nimport subprocess, sys\np = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\nwith open('child.pid', 'w') as f:\n    f.write(str(p.pid))\nsys.exit(0)\n",
        );

        let mut module = descriptor();
        module.id = format!("v6desc{}", std::process::id());
        module.module_dir = root.clone();
        module.entrypoint = entrypoint;
        let process = V6Process::start(module, Arc::new(NoopExecutor), 1)
            .await
            .unwrap();

        let pid_path = root.join("child.pid");
        for _ in 0..100 {
            if pid_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let descendant: i32 = fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        for _ in 0..100 {
            if process.status() == super::super::process::ProcessStatus::Crashed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            process.status(),
            super::super::process::ProcessStatus::Crashed
        );

        fn descendant_is_live(pid: i32) -> bool {
            let proc_stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            match proc_stat {
                Ok(stat) => {
                    // `/proc/<pid>/stat` is `pid (comm) state ...`; comm may
                    // contain spaces or parentheses, so find the final `) `.
                    // A zombie has already been killed and cannot execute or
                    // retain module resources; in a PID namespace its init is
                    // responsible for the eventual wait/reap.
                    let Some(after_comm) = stat.rsplit_once(") ").map(|(_, tail)| tail) else {
                        return true;
                    };
                    !after_comm.starts_with('Z')
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => true,
            }
        }

        let mut alive = true;
        for _ in 0..200 {
            alive = descendant_is_live(descendant);
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if alive {
            unsafe {
                let _ = libc::kill(descendant, libc::SIGKILL);
            }
        }
        let _ = fs::remove_dir_all(&root);
        assert!(
            !alive,
            "live descendant process survived v6 supervisor cleanup"
        );
    }
}
