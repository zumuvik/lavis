use super::{
    manifest::{ExternalCapability, ExternalModuleDescriptor},
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
    sync::Arc,
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
const V6_MAX_STDERR_CAPTURE: usize = 16 * 1024;

/// Cloneable front end to the single-owner V6 child-process actor.
#[derive(Clone)]
pub(crate) struct V6Process {
    control: mpsc::Sender<Control>,
    descriptor: Arc<ExternalModuleDescriptor>,
}

impl V6Process {
    pub(crate) async fn start(
        descriptor: ExternalModuleDescriptor,
        executor: Arc<dyn V6TelegramExecutor>,
    ) -> Result<Self, ExternalError> {
        if descriptor.protocol_version != 6
            || !descriptor.entrypoint.starts_with(&descriptor.module_dir)
        {
            return Err(ExternalError::InvalidArgument);
        }

        let state_dir = v6_module_state_dir(&descriptor.id, &|name| std::env::var_os(name))
            .ok_or(ExternalError::StateRead)?;
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
        let stderr_drain = tokio::spawn(drain_stderr(stderr));
        tokio::spawn(supervise(
            child,
            process_group,
            descriptor.clone(),
            executor,
            SupervisorIo {
                control_rx,
                reader_rx,
                writer_tx,
                rpc_rx,
                actor_tx: rpc_tx,
                reader,
                writer,
                stderr_drain,
            },
        ));
        Ok(Self {
            control: control_tx,
            descriptor: Arc::new(descriptor),
        })
    }

    pub(crate) fn descriptor(&self) -> &ExternalModuleDescriptor {
        &self.descriptor
    }

    pub(crate) fn status(&self) -> super::process::ProcessStatus {
        if self.control.is_closed() {
            super::process::ProcessStatus::Crashed
        } else {
            super::process::ProcessStatus::Running
        }
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
        let _ = self.graceful_shutdown().await;
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expected {
    Initialized,
    Health,
    Result,
    EventResult,
}

struct Pending {
    expected: Expected,
    deadline: Instant,
    reply: oneshot::Sender<Result<V6InboundFrame, ExternalError>>,
}

enum WriterCommand {
    Frame(V6OutboundCoreFrame, Flush),
}

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
    stderr_drain: JoinHandle<Vec<u8>>,
}

async fn supervise(
    mut child: Child,
    process_group: u32,
    descriptor: ExternalModuleDescriptor,
    executor: Arc<dyn V6TelegramExecutor>,
    io: SupervisorIo,
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
    } = io;
    let mut pending = HashMap::new();
    let mut active_calls = HashSet::new();
    let mut workers = JoinSet::new();
    let mut closing = false;
    let mut shutdown_reply: Option<oneshot::Sender<Result<(), ExternalError>>> = None;
    let mut shutdown_deadline = None;
    let mut shutdown_flushed = false;
    let mut stored_child_exit = None;
    let mut child_reaped = false;
    let mut reader_open = true;
    let mut control_open = true;
    let mut force_kill = false;
    loop {
        tokio::select! {
            child_exit = child.wait(), if stored_child_exit.is_none() => {
                child_reaped = child_exit.is_ok();
                if closing {
                    // The writer may already own and successfully flush the
                    // shutdown line. Do not classify this exit until it tells
                    // us definitively whether that flush happened.
                    stored_child_exit = Some(child_exit);
                    if shutdown_flushed {
                        let Some(child_exit) = stored_child_exit.take() else {
                            break;
                        };
                        if let Some(reply) = shutdown_reply.take() {
                            let _ = reply.send(shutdown_child_exit_result(true, child_exit));
                        }
                        break;
                    }
                } else {
                    break;
                }
            }
            control = control_rx.recv(), if control_open => match control {
                Some(Control::Request { frame, expected, reply }) => {
                    if closing { let _ = reply.send(Err(ExternalError::Unavailable)); continue; }
                    let Some(request_id) = request_id(&frame).map(str::to_owned) else { let _ = reply.send(Err(ExternalError::ProtocolEncode)); continue; };
                    if pending.contains_key(&request_id) { let _ = reply.send(Err(ExternalError::WrongRequestId)); continue; }
                    if pending.len() == V6_MAX_PENDING { let _ = reply.send(Err(ExternalError::Unavailable)); continue; }
                    if queue_writer(&writer_tx, WriterCommand::Frame(frame, Flush::None)).is_err() {
                        let _ = reply.send(Err(ExternalError::Unavailable));
                        break;
                    }
                    pending.insert(request_id, Pending { expected, deadline: Instant::now() + V6_LIFECYCLE_TIMEOUT, reply });
                }
                Some(Control::Shutdown { request_id, reply }) => {
                    if closing { let _ = reply.send(Err(ExternalError::Unavailable)); }
                    else if !start_shutdown(&mut closing, &mut pending, &mut workers, &writer_tx, request_id, Some(reply), &mut shutdown_reply) { break; }
                },
                None => {
                    control_open = false;
                    if !closing
                        && !start_shutdown(&mut closing, &mut pending, &mut workers, &writer_tx, "0".to_owned(), None, &mut shutdown_reply)
                    {
                        break;
                    }
                },
            },
            event = reader_rx.recv(), if reader_open => match event {
                Some(ActorEvent::Inbound(Ok(frame))) => match frame {
                    V6InboundFrame::TelegramInvoke(V6ModuleFrame::TelegramInvoke { call_id, method, params }) => {
                        if !reserve_call_id(&mut active_calls, &call_id) {
                            // Duplicate call IDs are protocol-fatal even while
                            // closing, so their meaning is never ambiguous.
                            break;
                        }
                        if closing {
                            let rejected = V6CallError { kind: "shutdown".to_owned(), message: "module is shutting down".to_owned() };
                            if queue_writer(&writer_tx, WriterCommand::Frame(V6OutboundCoreFrame::TelegramResult { call_id: call_id.clone(), result: Err(rejected) }, Flush::Call(call_id))).is_err() { break; }
                            continue;
                        }
                        if active_calls.len() > V6_MAX_ACTIVE_RPCS {
                            let error = V6CallError { kind: "capacity".to_owned(), message: "too many active calls".to_owned() };
                            if queue_writer(&writer_tx, WriterCommand::Frame(V6OutboundCoreFrame::TelegramResult { call_id: call_id.clone(), result: Err(error) }, Flush::Call(call_id))).is_err() { break; }
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
                                        Err(_) => Err(V6ExecutorError::Transport),
                                    };
                                    let _ = tx.send(ActorEvent::RpcComplete { call_id, result: map_executor_result(result) }).await;
                                });
                            }
                            Err(error) => {
                                if queue_writer(&writer_tx, WriterCommand::Frame(V6OutboundCoreFrame::TelegramResult { call_id: call_id.clone(), result: Err(error) }, Flush::Call(call_id))).is_err() { break; }
                            }
                        }
                    }
                    frame if terminal_request_id(&frame).is_some() => {
                        let Some(request_id) = terminal_request_id(&frame) else {
                            break;
                        };
                        if let Some(pending_request) = pending.remove(request_id) {
                            if !initialized_module_matches(&descriptor, &frame) { let _ = pending_request.reply.send(Err(ExternalError::WrongModuleId)); break; }
                            else if expected_matches(pending_request.expected, &frame) { let _ = pending_request.reply.send(Ok(frame)); }
                            else { let _ = pending_request.reply.send(Err(ExternalError::ProtocolDecode)); }
                        } else { break; }
                    }
                    V6InboundFrame::Log { .. } => {}
                    _ => break,
                },
                Some(ActorEvent::Inbound(Err(_))) => break,
                Some(ActorEvent::ReaderEof) | None => {
                    reader_open = false;
                    if reader_eof_is_fatal(closing) {
                        if let Some(reply) = shutdown_reply.take() {
                            let _ = reply.send(Err(ExternalError::Unavailable));
                        }
                        break;
                    }
                },
                _ => {}
            },
            event = rpc_rx.recv() => match event {
                Some(ActorEvent::RpcComplete { call_id, result }) => {
                    if queue_writer(&writer_tx, WriterCommand::Frame(V6OutboundCoreFrame::TelegramResult { call_id: call_id.clone(), result }, Flush::Call(call_id))).is_err() { break; }
                }
                Some(ActorEvent::Flushed(call_id)) => { active_calls.remove(&call_id); }
                Some(ActorEvent::ShutdownFlushed) => {
                    shutdown_flushed = true;
                    shutdown_deadline = Some(Instant::now() + V6_SHUTDOWN_TIMEOUT);
                    if let Some(child_exit) = stored_child_exit.take() {
                        if let Some(reply) = shutdown_reply.take() {
                            let _ = reply.send(shutdown_child_exit_result(true, child_exit));
                        }
                        break;
                    }
                }
                Some(ActorEvent::WriterFailed) | None => { if let Some(reply) = shutdown_reply.take() { let _ = reply.send(Err(ExternalError::Unavailable)); } break; },
                _ => {}
            },
            _ = async { if let Some(deadline) = shutdown_deadline { tokio::time::sleep_until(deadline).await } else { future::pending::<()>().await } }, if closing => {
                if let Some(reply) = shutdown_reply.take() { let _ = reply.send(Err(ExternalError::ShutdownTimeout)); }
                kill_group(process_group);
                force_kill = true;
                break;
            },
            _ = sleep_until_pending_deadline(&pending), if !pending.is_empty() => {
                expire_pending(&mut pending);
            },
        }
        while workers.try_join_next().is_some() {}
    }
    fail_pending(&mut pending);
    workers.abort_all();
    while workers.join_next().await.is_some() {}
    drop(writer_tx);
    reader.abort();
    writer.abort();
    stderr_drain.abort();
    let _ = reader.await;
    let _ = writer.await;
    let _ = stderr_drain.await;
    if !child_reaped {
        if force_kill || timeout(V6_SHUTDOWN_TIMEOUT, child.wait()).await.is_err() {
            kill_group(process_group);
        }
        let _ = child.wait().await;
    }
}

fn queue_writer(writer: &mpsc::Sender<WriterCommand>, command: WriterCommand) -> Result<(), ()> {
    writer.try_send(command).map_err(|_| ())
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
) -> bool {
    if *closing {
        // A dropped control sender must not overwrite a live shutdown reply.
        return true;
    }
    *closing = true;
    fail_pending(pending);
    workers.abort_all();
    if queue_writer(
        writer,
        WriterCommand::Frame(
            V6OutboundCoreFrame::Shutdown { request_id },
            Flush::Shutdown,
        ),
    )
    .is_err()
    {
        if let Some(reply) = reply {
            let _ = reply.send(Err(ExternalError::Unavailable));
        }
        return false;
    }
    *shutdown_reply = reply;
    true
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
        if let Some(event) = event {
            if tx.send(event).await.is_err() {
                return;
            }
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
    if !descriptor
        .capabilities
        .contains(&ExternalCapability::TelegramRaw)
    {
        return Err(V6CallError {
            kind: "capability".to_owned(),
            message: "telegram.raw capability is required".to_owned(),
        });
    }
    let method = v6_registry::lookup(method).ok_or_else(|| V6CallError {
        kind: "validation".to_owned(),
        message: "method is not allowlisted".to_owned(),
    })?;
    if !descriptor.telegram_methods.contains(&method) {
        return Err(V6CallError {
            kind: "capability".to_owned(),
            message: "method is not granted".to_owned(),
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
        },
        V6ExecutorError::Rpc { .. } => V6CallError {
            kind: "rpc".to_owned(),
            message: "Telegram RPC request failed".to_owned(),
        },
        V6ExecutorError::Transport => V6CallError {
            kind: "transport".to_owned(),
            message: "Telegram transport unavailable".to_owned(),
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
    child_exit
        .map(|_| ())
        .map_err(|_| ExternalError::Unavailable)
}

fn reader_eof_is_fatal(closing: bool) -> bool {
    !closing
}

fn expire_pending(pending: &mut HashMap<String, Pending>) {
    let now = Instant::now();
    let retained = std::mem::take(pending);
    for (request_id, request) in retained {
        if request.deadline <= now {
            let _ = request.reply.send(Err(ExternalError::ExecutionTimeout));
        } else {
            pending.insert(request_id, request);
        }
    }
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

async fn drain_stderr(mut stderr: tokio::process::ChildStderr) -> Vec<u8> {
    let mut captured = Vec::with_capacity(V6_MAX_STDERR_CAPTURE);
    let mut chunk = [0u8; 1024];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return captured,
            Ok(read) => {
                let remaining = V6_MAX_STDERR_CAPTURE.saturating_sub(captured.len());
                captured.extend_from_slice(&chunk[..read.min(remaining)]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    #[test]
    fn ungranted_method_is_rejected_before_executor() {
        let descriptor = descriptor();
        assert!(
            matches!(validate_invoke(&descriptor, "account.updateStatus", true), Err(V6CallError { kind, .. }) if kind == "capability")
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
        expire_pending(&mut pending);
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
}
