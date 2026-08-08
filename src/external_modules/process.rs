use super::{
    gateway::{GatewayContext, TELEGRAM_CALL_TIMEOUT, TelegramGateway},
    manifest::{ExternalCapability, ExternalModuleDescriptor},
    protocol::{
        self, CoreMessage, MAX_LINE_BYTES, MAX_RESULT_BYTES, MessageEvent, MessageEventKind,
        ModuleMessage,
    },
};
use crate::error::ExternalError;
use std::{
    collections::HashSet,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    time::{Instant, timeout, timeout_at},
};

pub const INIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Total budget for a v5 parent request, including its optional nested call.
pub const PARENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Legacy v2-v4 execute/event deadline. It remains unchanged.
pub const EXECUTE_TIMEOUT: Duration = Duration::from_secs(5);
/// Once the core has answered `telegram.result`, the module gets this bounded
/// time to send the correlated terminal reply.
pub const TERMINAL_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// Reserved for flushing `telegram.result` before terminal-reply waiting begins.
pub const TELEGRAM_RESULT_WRITE_RESERVE: Duration = Duration::from_millis(250);
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_STDERR_CAPTURE: usize = 16 * 1024;
/// How long to wait for the stderr reader to drain bytes already buffered in
/// the kernel pipe before aborting it. Bounded so cleanup never blocks on a
/// descendant that retains the inherited stderr FD.
pub const STDERR_DRAIN_GRACE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Failed,
    Crashed,
    Terminated,
}

pub struct ModuleProcess {
    child: Option<Child>,
    process_group_id: Option<u32>,
    stdin: tokio::process::ChildStdin,
    stdout_reader: tokio::io::BufReader<tokio::process::ChildStdout>,
    stderr_drain: Option<tokio::task::JoinHandle<()>>,
    stderr_capture: Arc<Mutex<StderrCapture>>,
    descriptor: ExternalModuleDescriptor,
    status: ProcessStatus,
    in_flight_request: Option<String>,
    gateway: Option<std::sync::Arc<dyn TelegramGateway>>,
    active_call_ids: HashSet<String>,
    telegram_invoke_parent: Option<String>,
    /// Test-only counter of crash events emitted for this process. Lets tests
    /// prove that normal shutdown never takes the crash path.
    #[cfg(test)]
    crash_events: std::sync::atomic::AtomicU32,
    /// Test-only record of the diagnostics emitted for the most recent crash.
    /// Per-process, so parallel tests never share state.
    #[cfg(test)]
    last_crash_diagnostics: std::sync::Mutex<Option<CrashDiagnostics>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct StderrCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl StderrCapture {
    /// Append a chunk, stopping once the capture limit is reached. Once the
    /// limit is hit the flag is set and further bytes are dropped, but the
    /// caller keeps draining the pipe so the module is never blocked.
    fn push(&mut self, chunk: &[u8]) {
        if self.truncated {
            return;
        }
        let remaining = MAX_STDERR_CAPTURE.saturating_sub(self.bytes.len());
        if chunk.len() <= remaining {
            self.bytes.extend_from_slice(chunk);
        } else {
            self.bytes.extend_from_slice(&chunk[..remaining]);
            self.truncated = true;
        }
    }

    /// Lossy UTF-8 view of the captured bytes. Never panics on malformed input.
    fn lossy_text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// Lock the shared capture, recovering from a poisoned mutex instead of
/// panicking. The guard is never held across an `.await`.
pub(crate) fn lock_capture(
    shared: &Mutex<StderrCapture>,
) -> std::sync::MutexGuard<'_, StderrCapture> {
    match shared.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl ModuleProcess {
    pub fn descriptor(&self) -> &ExternalModuleDescriptor {
        &self.descriptor
    }

    pub fn status(&self) -> ProcessStatus {
        self.status
    }

    pub fn id(&self) -> &str {
        &self.descriptor.id
    }

    pub fn in_flight_request(&self) -> Option<&str> {
        self.in_flight_request.as_deref()
    }

    pub async fn start(descriptor: ExternalModuleDescriptor) -> Result<Self, ExternalError> {
        Self::start_with_gateway(descriptor, None).await
    }
    pub async fn start_with_gateway(
        descriptor: ExternalModuleDescriptor,
        gateway: Option<std::sync::Arc<dyn TelegramGateway>>,
    ) -> Result<Self, ExternalError> {
        let entrypoint = descriptor.entrypoint.clone();
        if !entrypoint.starts_with(&descriptor.module_dir) {
            return Err(ExternalError::PathEscape);
        }
        let state_dir = module_state_dir(&descriptor.id, &|name| std::env::var_os(name))
            .ok_or(ExternalError::StateRead)?;
        tokio::fs::create_dir_all(&state_dir)
            .await
            .map_err(|_| ExternalError::StateWrite)?;
        secure_directory(&state_dir)
            .await
            .map_err(|_| ExternalError::StateWrite)?;

        let mut command = Command::new(&entrypoint);
        command
            .current_dir(&descriptor.module_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            // Do not inherit the host search path (or any credentials it may
            // expose through wrappers). Entrypoints must be executable paths.
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
                    let ret = libc::setsid();
                    if ret == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let mut child = command.spawn().map_err(|_| ExternalError::Unavailable)?;

        let Some(pid) = child.id() else {
            cleanup_spawned_child(&mut child, None).await;
            return Err(ExternalError::Unavailable);
        };

        let Some(stdin) = child.stdin.take() else {
            cleanup_spawned_child(&mut child, Some(pid)).await;
            return Err(ExternalError::Unavailable);
        };
        let Some(stderr) = child.stderr.take() else {
            cleanup_spawned_child(&mut child, Some(pid)).await;
            return Err(ExternalError::Unavailable);
        };
        let Some(stdout) = child.stdout.take() else {
            cleanup_spawned_child(&mut child, Some(pid)).await;
            return Err(ExternalError::Unavailable);
        };
        let stdout_reader = BufReader::new(stdout);

        let stderr_capture = Arc::new(Mutex::new(StderrCapture::default()));
        let stderr_drain = tokio::spawn(drain_stderr(stderr, stderr_capture.clone()));

        let mut process = Self {
            child: Some(child),
            process_group_id: Some(pid),
            stdin,
            stdout_reader,
            stderr_drain: Some(stderr_drain),
            stderr_capture,
            descriptor,
            status: ProcessStatus::Running,
            in_flight_request: None,
            gateway,
            active_call_ids: HashSet::new(),
            telegram_invoke_parent: None,
            #[cfg(test)]
            crash_events: std::sync::atomic::AtomicU32::new(0),
            #[cfg(test)]
            last_crash_diagnostics: std::sync::Mutex::new(None),
        };

        // Every error path of `handshake()` already runs `fail_and_terminate`,
        // which stops the process group, reaps the child and joins the stderr
        // reader. Doing that again here would be redundant cleanup.
        process.handshake().await?;

        Ok(process)
    }

    async fn handshake(&mut self) -> Result<(), ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Initialize {
            request_id: req_id.clone(),
            module_id: self.descriptor.id.clone(),
        };
        self.in_flight_request = Some(req_id.clone());
        if let Err(e) = self.send(&msg).await {
            return Err(self.fail_and_terminate(e).await);
        }

        // A single absolute deadline for the whole handshake. Log messages do
        // not extend it, so a module that only logs can never stall startup.
        let deadline = Instant::now() + INIT_TIMEOUT;
        loop {
            let response = match timeout_at(deadline, self.read_message()).await {
                Ok(inner) => match inner {
                    Ok(msg) => msg,
                    Err(e) => return Err(self.fail_and_terminate(e).await),
                },
                Err(_) => {
                    return Err(self
                        .fail_and_terminate(ExternalError::HandshakeTimeout)
                        .await);
                }
            };

            match response {
                ModuleMessage::Log {
                    request_id: log_id,
                    level,
                    message,
                } => {
                    if log_id != req_id {
                        return Err(self.fail_and_terminate(ExternalError::WrongRequestId).await);
                    }
                    log_module_message(&self.descriptor.id, &log_id, &level, &message);
                    // Keep waiting against the original deadline.
                }
                ModuleMessage::Initialized {
                    request_id,
                    module_id,
                } => {
                    if request_id != req_id {
                        return Err(self.fail_and_terminate(ExternalError::WrongRequestId).await);
                    }
                    if module_id != self.descriptor.id {
                        return Err(self.fail_and_terminate(ExternalError::WrongModuleId).await);
                    }
                    self.clear_request_state();
                    return Ok(());
                }
                ModuleMessage::Error { request_id, .. } => {
                    if request_id != req_id {
                        return Err(self.fail_and_terminate(ExternalError::WrongRequestId).await);
                    }
                    return Err(self.fail_and_terminate(ExternalError::ModuleError).await);
                }
                _ => return Err(self.fail_and_terminate(ExternalError::ProtocolDecode).await),
            }
        }
    }

    pub async fn execute(
        &mut self,
        command: &str,
        arguments: &str,
    ) -> Result<String, ExternalError> {
        self.execute_with_entities(command, arguments, &[]).await
    }

    pub async fn execute_with_entities(
        &mut self,
        command: &str,
        arguments: &str,
        argument_entities: &[protocol::CustomEmojiEntity],
    ) -> Result<String, ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Execute {
            request_id: req_id.clone(),
            command: command.to_owned(),
            arguments: arguments.to_owned(),
            argument_entities: argument_entities.to_vec(),
        };
        self.in_flight_request = Some(req_id.clone());
        if let Err(e) = self.send(&msg).await {
            return Err(self.fail_and_terminate(e).await);
        }

        let result = match self
            .collect_reply_until(&req_id, request_deadline(self.descriptor.protocol_version))
            .await
        {
            Ok(msg) => msg,
            Err(error) => return Err(self.fail_and_terminate(error).await),
        };

        match result {
            ModuleMessage::Result { request_id, text } => {
                if request_id != req_id {
                    return Err(self.fail_and_terminate(ExternalError::WrongRequestId).await);
                }
                self.clear_request_state();
                Ok(truncate_result(&text))
            }
            ModuleMessage::Error {
                request_id,
                code: _,
                message: _,
            } => {
                if request_id != req_id {
                    return Err(self.fail_and_terminate(ExternalError::WrongRequestId).await);
                }
                // A protocol-valid correlated application error is not a crash
                // and does not terminate the module: clear the request-scoped
                // state and leave the process Running so it can serve the next
                // request. No `external_module_crashed` is emitted.
                self.clear_request_state();
                Err(ExternalError::ModuleError)
            }
            _ => Err(self.fail_and_terminate(ExternalError::ProtocolDecode).await),
        }
    }

    pub async fn dispatch_event(
        &mut self,
        event: MessageEventKind,
        payload: MessageEvent,
    ) -> Result<(String, Vec<protocol::EventAction>), ExternalError> {
        if self.descriptor.protocol_version < 3
            || (event == MessageEventKind::Edited && self.descriptor.protocol_version < 4)
        {
            return Err(ExternalError::InvalidArgument);
        }
        let request_id = protocol::request_id();
        self.in_flight_request = Some(request_id.clone());
        let message = CoreMessage::Event {
            request_id: request_id.clone(),
            event,
            payload,
        };
        if let Err(error) = self.send(&message).await {
            return Err(self.fail_and_terminate(error).await);
        }
        let reply = match self
            .collect_reply_until(
                &request_id,
                request_deadline(self.descriptor.protocol_version),
            )
            .await
        {
            Ok(reply) => reply,
            Err(error) => return Err(self.fail_and_terminate(error).await),
        };
        match reply {
            ModuleMessage::EventResult {
                request_id: actual,
                actions,
            } if actual == request_id => {
                self.clear_request_state();
                Ok((request_id, actions))
            }
            ModuleMessage::EventResult { .. } => {
                Err(self.fail_and_terminate(ExternalError::WrongRequestId).await)
            }
            _ => Err(self.fail_and_terminate(ExternalError::ProtocolDecode).await),
        }
    }

    async fn collect_reply_until(
        &mut self,
        expected_id: &str,
        parent_deadline: Instant,
    ) -> Result<ModuleMessage, ExternalError> {
        let mut read_deadline = parent_deadline;
        loop {
            let line = timeout_at(read_deadline, self.read_line())
                .await
                .map_err(|_| ExternalError::ExecutionTimeout)??;
            let Some(msg) = line else {
                self.status = ProcessStatus::Crashed;
                return Err(ExternalError::Unavailable);
            };
            match msg {
                ModuleMessage::Log {
                    request_id,
                    level,
                    message,
                } => {
                    if request_id != expected_id {
                        return Err(ExternalError::WrongRequestId);
                    }
                    log_module_message(&self.descriptor.id, &request_id, &level, &message);
                    continue;
                }
                ModuleMessage::TelegramInvoke {
                    request_id,
                    call_id,
                    method,
                    params,
                } => {
                    if !allows_telegram_invoke(
                        self.descriptor.protocol_version,
                        self.in_flight_request.as_deref(),
                        self.telegram_invoke_parent.as_deref(),
                        expected_id,
                        &request_id,
                    ) || !self.active_call_ids.insert(call_id.clone())
                    {
                        return Err(ExternalError::ProtocolDecode);
                    }
                    self.telegram_invoke_parent = Some(expected_id.to_owned());
                    let result = if !self
                        .descriptor
                        .capabilities
                        .contains(&ExternalCapability::TelegramAccountStatus)
                    {
                        Err(telegram_error(
                            "capability",
                            "telegram.account.status capability is required",
                        ))
                    } else if let Some(gateway) = &self.gateway {
                        let context = GatewayContext {
                            module_id: self.descriptor.id.clone(),
                            request_id: expected_id.to_owned(),
                        };
                        if !has_nested_call_budget(parent_deadline, Instant::now()) {
                            Err(telegram_error(
                                "timeout",
                                "parent request deadline has insufficient nested-call budget",
                            ))
                        } else {
                            match timeout(
                                TELEGRAM_CALL_TIMEOUT,
                                gateway.invoke(context, &method, params),
                            )
                            .await
                            {
                                Ok(result) => result,
                                Err(_) => {
                                    Err(telegram_error("timeout", "Telegram request timed out"))
                                }
                            }
                        }
                    } else {
                        Err(telegram_error("transport", "Telegram gateway unavailable"))
                    };
                    self.active_call_ids.remove(&call_id);
                    self.send(&CoreMessage::TelegramResult {
                        request_id,
                        call_id,
                        result,
                    })
                    .await?;
                    read_deadline =
                        std::cmp::min(parent_deadline, Instant::now() + TERMINAL_REPLY_TIMEOUT);
                    continue;
                }
                ModuleMessage::Result { ref request_id, .. }
                | ModuleMessage::Error { ref request_id, .. }
                | ModuleMessage::Health { ref request_id }
                | ModuleMessage::EventResult { ref request_id, .. } => {
                    if *request_id == *expected_id {
                        return Ok(msg);
                    }
                    return Err(ExternalError::WrongRequestId);
                }
                _ => return Err(ExternalError::ProtocolDecode),
            }
        }
    }

    async fn collect_reply(&mut self, expected_id: &str) -> Result<ModuleMessage, ExternalError> {
        self.collect_reply_until(expected_id, Instant::now() + HEALTH_TIMEOUT)
            .await
    }

    pub async fn health_check(&mut self) -> Result<(), ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Health {
            request_id: req_id.clone(),
        };
        self.in_flight_request = Some(req_id.clone());
        if let Err(e) = self.send(&msg).await {
            return Err(self.fail_and_terminate(e).await);
        }

        let response = match timeout(HEALTH_TIMEOUT, self.collect_reply(&req_id)).await {
            Ok(inner) => match inner {
                Ok(msg) => msg,
                Err(ExternalError::ExecutionTimeout) => {
                    return Err(self.fail_and_terminate(ExternalError::HealthTimeout).await);
                }
                Err(e) => return Err(self.fail_and_terminate(e).await),
            },
            Err(_) => {
                return Err(self.fail_and_terminate(ExternalError::HealthTimeout).await);
            }
        };

        match response {
            ModuleMessage::Health { request_id } => {
                if request_id != req_id {
                    return Err(self.fail_and_terminate(ExternalError::WrongRequestId).await);
                }
                self.clear_request_state();
                Ok(())
            }
            _ => Err(self.fail_and_terminate(ExternalError::ProtocolDecode).await),
        }
    }

    pub async fn graceful_shutdown(&mut self) -> Result<(), ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Shutdown {
            request_id: req_id.clone(),
        };
        self.in_flight_request = Some(req_id.clone());
        if let Err(error) = self.send(&msg).await {
            return Err(self.fail_and_terminate(error).await);
        }

        match timeout(SHUTDOWN_TIMEOUT, self.reap_child()).await {
            Ok(()) => {
                self.join_stderr_drain().await;
                self.clear_request_state();
                self.status = ProcessStatus::Terminated;
                Ok(())
            }
            Err(_) => Err(self
                .fail_and_terminate(ExternalError::ShutdownTimeout)
                .await),
        }
    }

    pub fn mark_failed(&mut self) {
        self.clear_request_state();
        self.status = ProcessStatus::Failed;
    }

    fn clear_request_state(&mut self) {
        self.in_flight_request = None;
        self.active_call_ids.clear();
        self.telegram_invoke_parent = None;
    }

    async fn fail_and_terminate(&mut self, error: ExternalError) -> ExternalError {
        let request_id = self.in_flight_request.take();
        self.clear_request_state();
        self.terminate_failed_process().await;
        let capture = self.snapshot_stderr();
        let diagnostics =
            build_crash_diagnostics(&self.descriptor, request_id.as_deref(), &error, &capture);
        #[cfg(test)]
        {
            *self.last_crash_diagnostics.lock().unwrap() = Some(diagnostics.clone());
            self.crash_events
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        emit_crash_event(&diagnostics);
        error
    }

    /// Stop and reap a fatally failed process without emitting diagnostics
    /// itself. `fail_and_terminate` uses this before snapshotting stderr and
    /// emitting the single `external_module_crashed` event.
    async fn terminate_failed_process(&mut self) {
        self.status = ProcessStatus::Crashed;
        self.terminate_process_group().await;
        self.reap_child().await;
        self.join_stderr_drain().await;
    }

    /// Snapshot the stderr accumulated so far. Safe to call after the reader
    /// task has been aborted: the shared buffer retains everything already read.
    fn snapshot_stderr(&self) -> StderrCapture {
        lock_capture(&self.stderr_capture).clone()
    }

    pub async fn terminate(&mut self) {
        self.status = ProcessStatus::Terminated;
        self.clear_request_state();
        self.terminate_process_group().await;
        self.reap_child().await;
        self.join_stderr_drain().await;
    }

    async fn terminate_process_group(&self) {
        #[cfg(unix)]
        {
            let Some(process_group_id) = self.process_group_id else {
                return;
            };
            let Ok(pgid) = i32::try_from(process_group_id) else {
                return;
            };
            let ret = unsafe { libc::kill(-pgid, libc::SIGKILL) };
            if ret == -1 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    tracing::warn!(
                        event = "process_group_kill_failed",
                        pid = process_group_id,
                        error = %err,
                        "Failed to kill process group"
                    );
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.process_group_id;
        }
    }

    async fn join_stderr_drain(&mut self) {
        if let Some(handle) = self.stderr_drain.take() {
            finish_stderr_drain(handle).await;
        }
    }

    async fn reap_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.wait().await;
        }
        // The process group ID is only valid while this ModuleProcess owns the
        // child. Clearing it after reaping prevents repeated cleanup from
        // signalling a PID that the kernel may have reused.
        self.process_group_id = None;
    }

    async fn send(&mut self, msg: &CoreMessage) -> Result<(), ExternalError> {
        let line = msg.serialize_for(self.descriptor.protocol_version)?;
        let mut full = line;
        full.push('\n');
        self.stdin
            .write_all(full.as_bytes())
            .await
            .map_err(|_| ExternalError::ProtocolEncode)?;
        self.stdin
            .flush()
            .await
            .map_err(|_| ExternalError::ProtocolEncode)?;
        Ok(())
    }

    async fn read_message(&mut self) -> Result<ModuleMessage, ExternalError> {
        let line = self.read_line().await?;
        line.ok_or(ExternalError::Unavailable)
    }

    async fn read_line(&mut self) -> Result<Option<ModuleMessage>, ExternalError> {
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        let mut single = [0u8; 1];
        loop {
            let n = self
                .stdout_reader
                .read(&mut single)
                .await
                .map_err(|_| ExternalError::ProtocolDecode)?;
            if n == 0 {
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            if single[0] == b'\n' {
                break;
            }
            if buf.len() >= MAX_LINE_BYTES {
                return Err(ExternalError::LineTooLarge);
            }
            buf.push(single[0]);
        }

        let trimmed = std::str::from_utf8(&buf).map_err(|_| ExternalError::ProtocolDecode)?;

        protocol::parse_module_line_for(trimmed, self.descriptor.protocol_version)
    }
}

pub(crate) async fn drain_stderr<R>(mut stderr: R, capture: Arc<Mutex<StderrCapture>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut tmp = [0u8; 1024];
    loop {
        let n = match stderr.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        // The guard is dropped at the end of this statement, before the next
        // read is awaited.
        lock_capture(&capture).push(&tmp[..n]);
    }
}

/// Give the stderr reader a bounded chance to consume bytes already buffered
/// in the kernel pipe before aborting it. Descendants can retain stderr after
/// the managed child exits; cleanup must never block on their inherited FD.
pub(crate) async fn finish_stderr_drain(mut handle: tokio::task::JoinHandle<()>) {
    if timeout(STDERR_DRAIN_GRACE, &mut handle).await.is_err() {
        handle.abort();
        let _ = handle.await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrashDiagnostics {
    pub(crate) module_id: String,
    pub(crate) protocol_version: u32,
    pub(crate) lifecycle_stage: String,
    pub(crate) request_id: String,
    pub(crate) error_category: String,
    pub(crate) error: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) stderr_truncated: bool,
    pub(crate) stderr: String,
    pub(crate) timestamp_unix_ms: u64,
    pub(crate) restart_generation: u64,
}

impl CrashDiagnostics {
    pub(crate) fn render_user(&self) -> String {
        let exit = match (self.exit_code, self.signal) {
            (Some(code), _) => format!("exit={code}"),
            (_, Some(signal)) => format!("signal={signal}"),
            _ => "exit=unknown".to_owned(),
        };
        let stderr = if self.stderr.is_empty() {
            "-".to_owned()
        } else {
            self.stderr.clone()
        };
        format!(
            "stage={}\ncategory={}\nrequest={}\n{}\ntimestamp_ms={}\ngeneration={}\nstderr_truncated={}\nstderr={}",
            self.lifecycle_stage,
            self.error_category,
            self.request_id,
            exit,
            self.timestamp_unix_ms,
            self.restart_generation,
            self.stderr_truncated,
            stderr,
        )
    }

    /// One-line health summary for `lm doctor`; never includes stderr or
    /// request payloads.
    pub(crate) fn summary(&self) -> String {
        format!(
            "stage={} category={} generation={}",
            self.lifecycle_stage, self.error_category, self.restart_generation
        )
    }
}

/// Backwards-compatible legacy diagnostic builder. V6 uses the richer context-aware
/// builder below so supervisor stage and process-exit details are retained.
pub(crate) fn build_crash_diagnostics(
    descriptor: &ExternalModuleDescriptor,
    request_id: Option<&str>,
    error: &ExternalError,
    capture: &StderrCapture,
) -> CrashDiagnostics {
    build_crash_diagnostics_with_context(
        descriptor,
        request_id,
        error,
        capture,
        CrashDiagnosticContext {
            lifecycle_stage: "runtime",
            error_category: "runtime_failure",
            exit_code: None,
            signal: None,
            restart_generation: 0,
        },
    )
}

pub(crate) struct CrashDiagnosticContext<'a> {
    pub(crate) lifecycle_stage: &'a str,
    pub(crate) error_category: &'a str,
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) restart_generation: u64,
}

pub(crate) fn build_crash_diagnostics_with_context(
    descriptor: &ExternalModuleDescriptor,
    request_id: Option<&str>,
    error: &ExternalError,
    capture: &StderrCapture,
    context: CrashDiagnosticContext<'_>,
) -> CrashDiagnostics {
    let timestamp_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    CrashDiagnostics {
        module_id: descriptor.id.clone(),
        protocol_version: descriptor.protocol_version,
        lifecycle_stage: context.lifecycle_stage.to_owned(),
        request_id: request_id.unwrap_or("-").to_owned(),
        error_category: context.error_category.to_owned(),
        error: error.to_string(),
        exit_code: context.exit_code,
        signal: context.signal,
        stderr_truncated: capture.truncated,
        stderr: capture.lossy_text().trim().to_owned(),
        timestamp_unix_ms,
        restart_generation: context.restart_generation,
    }
}

pub(crate) fn emit_crash_event(diagnostics: &CrashDiagnostics) {
    tracing::error!(
        event = "external_module_crashed",
        module_id = %diagnostics.module_id,
        protocol_version = diagnostics.protocol_version,
        lifecycle_stage = %diagnostics.lifecycle_stage,
        request_id = %diagnostics.request_id,
        error_category = %diagnostics.error_category,
        error = %diagnostics.error,
        exit_code = ?diagnostics.exit_code,
        signal = ?diagnostics.signal,
        timestamp_unix_ms = diagnostics.timestamp_unix_ms,
        restart_generation = diagnostics.restart_generation,
        stderr_truncated = diagnostics.stderr_truncated,
        stderr = %diagnostics.stderr,
        "External module crashed"
    );
}

pub(crate) fn log_module_message(module_id: &str, request_id: &str, level: &str, message: &str) {
    match level {
        "error" => tracing::error!(
            event = "external_module_log",
            module_id = %module_id,
            request_id = %request_id,
            message = %message,
            "External module log"
        ),
        "warn" | "warning" => tracing::warn!(
            event = "external_module_log",
            module_id = %module_id,
            request_id = %request_id,
            message = %message,
            "External module log"
        ),
        "debug" => tracing::debug!(
            event = "external_module_log",
            module_id = %module_id,
            request_id = %request_id,
            message = %message,
            "External module log"
        ),
        "trace" => tracing::trace!(
            event = "external_module_log",
            module_id = %module_id,
            request_id = %request_id,
            message = %message,
            "External module log"
        ),
        _ => tracing::info!(
            event = "external_module_log",
            module_id = %module_id,
            request_id = %request_id,
            message = %message,
            "External module log"
        ),
    }
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

fn truncate_result(text: &str) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        text.to_owned()
    } else {
        let mut end = 0;
        for (count, (idx, _)) in text.char_indices().enumerate() {
            if count >= MAX_RESULT_BYTES {
                break;
            }
            end = idx;
        }
        format!("{}…", &text[..end])
    }
}

fn telegram_error(kind: &'static str, message: &str) -> protocol::TelegramCallError {
    protocol::TelegramCallError {
        kind,
        code: None,
        name: None,
        message: message.to_owned(),
        retry_after_seconds: None,
    }
}

fn has_nested_call_budget(parent_deadline: Instant, now: Instant) -> bool {
    parent_deadline.saturating_duration_since(now)
        >= TELEGRAM_CALL_TIMEOUT + TELEGRAM_RESULT_WRITE_RESERVE + TERMINAL_REPLY_TIMEOUT
}

fn request_deadline(protocol_version: u32) -> Instant {
    Instant::now() + request_timeout(protocol_version)
}

fn request_timeout(protocol_version: u32) -> Duration {
    if protocol_version == 5 {
        PARENT_REQUEST_TIMEOUT
    } else {
        EXECUTE_TIMEOUT
    }
}

fn allows_telegram_invoke(
    protocol_version: u32,
    active_parent: Option<&str>,
    invoked_parent: Option<&str>,
    expected_parent: &str,
    supplied_parent: &str,
) -> bool {
    protocol_version == 5
        && active_parent == Some(expected_parent)
        && supplied_parent == expected_parent
        && invoked_parent != Some(expected_parent)
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[test]
    fn parent_timeout_reserves_nested_gateway_and_terminal_reply_budget() {
        let now = Instant::now();
        let nested_budget =
            TELEGRAM_CALL_TIMEOUT + TELEGRAM_RESULT_WRITE_RESERVE + TERMINAL_REPLY_TIMEOUT;
        assert!(PARENT_REQUEST_TIMEOUT > nested_budget);
        assert!(has_nested_call_budget(now + nested_budget, now));
        assert!(!has_nested_call_budget(
            now + nested_budget - Duration::from_millis(1),
            now
        ));
    }

    #[test]
    fn legacy_protocols_keep_the_five_second_request_deadline() {
        for protocol_version in 2..=4 {
            assert_eq!(request_timeout(protocol_version), EXECUTE_TIMEOUT);
        }
        assert_eq!(request_timeout(5), PARENT_REQUEST_TIMEOUT);
    }

    #[test]
    fn invoke_is_bound_to_its_parent_and_limited_to_one_total_call() {
        assert!(allows_telegram_invoke(5, Some("10"), None, "10", "10"));
        assert!(!allows_telegram_invoke(
            5,
            Some("10"),
            Some("10"),
            "10",
            "10"
        ));
        assert!(!allows_telegram_invoke(5, Some("10"), None, "10", "11"));
        assert!(!allows_telegram_invoke(4, Some("10"), None, "10", "10"));
        assert!(!allows_telegram_invoke(5, None, None, "10", "10"));
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use tokio::io::AsyncWriteExt;

    fn descriptor(id: &str) -> ExternalModuleDescriptor {
        ExternalModuleDescriptor {
            protocol_version: 2,
            id: id.to_owned(),
            display_name: id.to_owned(),
            version: "0.1.0".to_owned(),
            author: "Test".to_owned(),
            entrypoint: PathBuf::from("/tmp/never-executed"),
            module_dir: PathBuf::from("/tmp/never-executed"),
            capabilities: Vec::new(),
            default_command: None,
            subscriptions: Vec::new(),
            telegram_methods: Vec::new(),
            actions: Vec::new(),
            commands: vec![],
        }
    }

    #[test]
    fn push_keeps_all_bytes_below_the_limit() {
        let mut capture = StderrCapture::default();
        capture.push(&[b'a'; 1024]);
        capture.push(&[b'b'; 1024]);
        assert_eq!(capture.bytes.len(), 2048);
        assert!(!capture.truncated);
    }

    #[test]
    fn push_truncates_at_the_limit_and_drops_further_bytes() {
        let mut capture = StderrCapture::default();
        capture.push(&[b'x'; MAX_STDERR_CAPTURE + 512]);
        assert_eq!(capture.bytes.len(), MAX_STDERR_CAPTURE);
        assert!(capture.truncated);
        // Bytes after the limit are dropped entirely.
        capture.push(&[b'y'; 64]);
        assert_eq!(capture.bytes.len(), MAX_STDERR_CAPTURE);
        assert!(capture.truncated);
    }

    #[test]
    fn invalid_utf8_is_replaced_without_panicking() {
        let mut capture = StderrCapture::default();
        capture.push(&[0xff, 0xfe, b'a']);
        let text = capture.lossy_text();
        assert!(text.contains('\u{FFFD}'));
        assert!(text.contains('a'));
    }

    #[test]
    fn poisoned_capture_mutex_recovers_without_panicking() {
        let capture = Arc::new(Mutex::new(StderrCapture::default()));
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = capture.lock().unwrap();
            panic!("poison the mutex");
        }));
        assert!(result.is_err());
        let snapshot = lock_capture(&capture).clone();
        assert!(snapshot.bytes.is_empty());
        assert!(!snapshot.truncated);
    }

    #[tokio::test]
    async fn drain_stderr_captures_everything_below_the_limit() {
        let (mut writer, reader) = tokio::io::duplex(MAX_STDERR_CAPTURE);
        let capture = Arc::new(Mutex::new(StderrCapture::default()));
        let task = tokio::spawn(drain_stderr(reader, capture.clone()));
        writer
            .write_all(b"first line\nsecond line\n")
            .await
            .unwrap();
        drop(writer);
        task.await.unwrap();
        let snapshot = lock_capture(&capture).clone();
        assert_eq!(snapshot.bytes, b"first line\nsecond line\n");
        assert!(!snapshot.truncated);
    }

    #[tokio::test]
    async fn drain_stderr_truncates_above_the_limit() {
        let (mut writer, reader) = tokio::io::duplex(MAX_STDERR_CAPTURE + 4096);
        let capture = Arc::new(Mutex::new(StderrCapture::default()));
        let task = tokio::spawn(drain_stderr(reader, capture.clone()));
        let payload = vec![b'x'; MAX_STDERR_CAPTURE + 2048];
        writer.write_all(&payload).await.unwrap();
        drop(writer);
        task.await.unwrap();
        let snapshot = lock_capture(&capture).clone();
        assert_eq!(snapshot.bytes.len(), MAX_STDERR_CAPTURE);
        assert!(snapshot.truncated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stderr_drain_gets_grace_to_consume_pending_bytes() {
        let (mut writer, reader) = tokio::io::duplex(128);

        writer.write_all(b"pending marker\n").await.unwrap();
        drop(writer);

        let capture = Arc::new(Mutex::new(StderrCapture::default()));
        let handle = tokio::spawn(drain_stderr(reader, capture.clone()));

        // Do not yield before this call. On the current-thread runtime the
        // spawned reader has not had an opportunity to run yet; the grace
        // helper itself must allow it to consume the buffered bytes before
        // cleanup completes.
        finish_stderr_drain(handle).await;

        let snapshot = lock_capture(&capture).clone();
        assert!(snapshot.lossy_text().contains("pending marker"));
    }

    #[test]
    fn crash_diagnostics_include_request_id_and_stderr() {
        let mut capture = StderrCapture::default();
        capture.push(b"  boom: kernel panic\n");
        let diagnostics = build_crash_diagnostics(
            &descriptor("diag"),
            Some("req-42"),
            &ExternalError::ProtocolDecode,
            &capture,
        );
        assert_eq!(diagnostics.module_id, "diag");
        assert_eq!(diagnostics.protocol_version, 2);
        assert_eq!(diagnostics.request_id, "req-42");
        assert_eq!(diagnostics.error, "protocol decode error");
        assert!(!diagnostics.stderr_truncated);
        assert_eq!(diagnostics.stderr, "boom: kernel panic");
    }

    #[test]
    fn crash_diagnostics_default_request_and_empty_stderr() {
        let capture = StderrCapture::default();
        let diagnostics = build_crash_diagnostics(
            &descriptor("diag"),
            None,
            &ExternalError::Unavailable,
            &capture,
        );
        assert_eq!(diagnostics.request_id, "-");
        assert_eq!(diagnostics.stderr, "");
        assert!(!diagnostics.stderr_truncated);
    }
}

fn module_state_dir<F>(module_id: &str, environment: &F) -> Option<PathBuf>
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
    if !base.is_absolute() {
        return None;
    }
    Some(base.join("lavis/modules").join(module_id))
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

pub async fn reap_child(mut child: Child) {
    let _ = child.wait().await;
}

#[cfg(all(test, feature = "fixture-tests"))]
mod tests {
    use super::*;
    use crate::external_modules::gateway::TelegramGateway;
    use std::env;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn test_nonce() -> String {
        format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        )
    }

    const ECHO_MODULE_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        args = val.get("arguments", "")
        resp = {"protocol_version": 2, "type": "result", "request_id": req_id, "text": args}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "health":
        resp = {"protocol_version": 2, "type": "health", "request_id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "shutdown":
        resp = {"protocol_version": 2, "type": "health", "request_id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
        break
"#;

    const CHILD_SPAWNER_PY: &str = r#"#!/usr/bin/env python3
import sys, json, subprocess, os
child = None
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        child = subprocess.Popen(["sh", "-c", "sleep 60"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        args = val.get("arguments", "")
        resp = {"protocol_version": 2, "type": "result", "request_id": req_id, "text": args}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "health":
        resp = {"protocol_version": 2, "type": "health", "request_id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "shutdown":
        resp = {"protocol_version": 2, "type": "health", "request_id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
        break
if child:
    child.kill()
"#;

    fn make_script(output: &Path, body: &str) {
        let python = python_executable();
        let body = body.replacen(
            "#!/usr/bin/env python3",
            &format!("#!{}", python.display()),
            1,
        );
        fs::write(output, body).unwrap();
        fs::set_permissions(output, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn python_executable() -> PathBuf {
        env::var_os("PATH")
            .into_iter()
            .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join("python3"))
            .find(|candidate| candidate.is_file())
            .expect("fixture tests require python3 in PATH")
    }

    fn create_echo_module() -> (ExternalModuleDescriptor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lavis-proc-test-{}", test_nonce()));
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(dir.join("bin"), fs::Permissions::from_mode(0o700)).unwrap();

        let fixture_path = dir.join("bin").join("echo-module");
        make_script(&fixture_path, ECHO_MODULE_PY);

        let descriptor = ExternalModuleDescriptor {
            protocol_version: 2,
            id: "echo".to_owned(),
            display_name: "Echo".to_owned(),
            version: "0.1.0".to_owned(),
            author: "Test".to_owned(),
            entrypoint: fixture_path,
            module_dir: dir.clone(),
            capabilities: Vec::new(),
            default_command: None,
            subscriptions: Vec::new(),
            telegram_methods: Vec::new(),
            actions: Vec::new(),
            commands: vec![],
        };
        (descriptor, dir)
    }

    fn create_child_spawner_module() -> (ExternalModuleDescriptor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lavis-proc-child-{}", test_nonce()));
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(dir.join("bin"), fs::Permissions::from_mode(0o700)).unwrap();

        let fixture_path = dir.join("bin").join("child-spawner");
        make_script(&fixture_path, CHILD_SPAWNER_PY);

        let descriptor = ExternalModuleDescriptor {
            protocol_version: 2,
            id: "child-spawner".to_owned(),
            display_name: "ChildSpawner".to_owned(),
            version: "0.1.0".to_owned(),
            author: "Test".to_owned(),
            entrypoint: fixture_path,
            module_dir: dir.clone(),
            capabilities: Vec::new(),
            default_command: None,
            subscriptions: Vec::new(),
            telegram_methods: Vec::new(),
            actions: Vec::new(),
            commands: vec![],
        };
        (descriptor, dir)
    }

    fn create_fixture_module(body: &str, id: &str) -> (ExternalModuleDescriptor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lavis-fixture-{}", test_nonce()));
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(dir.join("bin"), fs::Permissions::from_mode(0o700)).unwrap();

        let fixture_path = dir.join("bin").join(id);
        make_script(&fixture_path, body);

        let descriptor = ExternalModuleDescriptor {
            protocol_version: 2,
            id: id.to_owned(),
            display_name: id.to_owned(),
            version: "0.1.0".to_owned(),
            author: "Test".to_owned(),
            entrypoint: fixture_path,
            module_dir: dir.clone(),
            capabilities: Vec::new(),
            default_command: None,
            subscriptions: Vec::new(),
            telegram_methods: Vec::new(),
            actions: Vec::new(),
            commands: vec![],
        };
        (descriptor, dir)
    }

    fn create_v3_event_module(id: &str) -> (ExternalModuleDescriptor, PathBuf) {
        let (mut descriptor, directory) = create_fixture_module(V3_EVENT_MODULE_PY, id);
        descriptor.protocol_version = 3;
        (descriptor, directory)
    }

    fn created_event() -> MessageEvent {
        MessageEvent {
            event_id: "event-1".to_owned(),
            message_ref: "message-1".to_owned(),
            message_key: "stable-message-1".to_owned(),
            peer_id: None,
            text: "hello".to_owned(),
            outgoing: true,
            entities: vec![],
        }
    }

    #[test]
    fn module_state_dir_uses_xdg_state_home_before_home() {
        let env = |name: &str| match name {
            "XDG_STATE_HOME" => Some(OsString::from("/tmp/state")),
            "HOME" => Some(OsString::from("/tmp/home")),
            _ => None,
        };
        assert_eq!(
            module_state_dir("gaf", &env),
            Some(PathBuf::from("/tmp/state/lavis/modules/gaf"))
        );
    }

    #[test]
    fn module_state_dir_falls_back_to_home_state() {
        let env = |name: &str| match name {
            "HOME" => Some(OsString::from("/tmp/home")),
            _ => None,
        };
        assert_eq!(
            module_state_dir("gaf", &env),
            Some(PathBuf::from("/tmp/home/.local/state/lavis/modules/gaf"))
        );
    }

    #[tokio::test]
    async fn test_handshake_success() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        assert_eq!(proc.status(), ProcessStatus::Running);
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_execute_command() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.execute("repeat", "Привет 🎉").await.unwrap();
        assert_eq!(result, "Привет 🎉");
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_health_check() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        proc.health_check().await.unwrap();
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_graceful_shutdown() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        proc.graceful_shutdown().await.unwrap();
        assert_eq!(proc.process_group_id, None);
        // Graceful shutdown must not take the crash path.
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 0);
        // Repeated cleanup after the child has exited must not retain a stale
        // PGID that could be reused by an unrelated process.
        proc.terminate().await;
        assert_eq!(proc.process_group_id, None);
        fs::remove_dir_all(&dir).unwrap();
    }

    const BAD_PROTO_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 1, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn test_wrong_protocol_version() {
        let (desc, dir) = create_fixture_module(BAD_PROTO_PY, "bad-proto");
        let result = ModuleProcess::start(desc).await;
        assert!(matches!(
            result,
            Err(ExternalError::ProtocolVersionMismatch)
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    const TIMEOUT_PY: &str = r#"#!/usr/bin/env python3
import sys, json, time
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        time.sleep(10)
        resp = {"protocol_version": 2, "type": "result", "request_id": req_id, "text": "too late"}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn test_execute_timeout() {
        let (desc, dir) = create_fixture_module(TIMEOUT_PY, "timeout");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.execute("repeat", "test").await;
        assert!(matches!(result, Err(ExternalError::ExecutionTimeout)));
        assert_eq!(proc.status(), ProcessStatus::Crashed);
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_terminate_process_group() {
        let (desc, dir) = create_child_spawner_module();
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    const V3_EVENT_MODULE_PY: &str = r#"#!/usr/bin/env python3
import json, sys, time
for line in sys.stdin:
    message = json.loads(line)
    request_id = message["request_id"]
    if message["type"] == "initialize":
        response = {"protocol_version": 3, "type": "initialized", "request_id": request_id, "module_id": message["module_id"]}
    elif message["type"] == "event":
        module_id = "" # replaced by the test fixture name
        message_ref = message["payload"]["message_ref"]
        if module_id == "timeout":
            time.sleep(10)
        if module_id == "exit":
            sys.exit(0)
        if module_id == "wrong-id":
            request_id = "999"
        if module_id == "ordinary":
            actions = [{"type": "message.react", "message_ref": message_ref, "reaction": {"type": "emoji", "emoji": "👍"}}]
        elif module_id == "custom":
            actions = [{"type": "message.react", "message_ref": message_ref, "reaction": {"type": "custom_emoji", "document_id": "5456140674028019486"}}]
        elif module_id == "malformed":
            actions = [{"type": "unexpected", "message_ref": message_ref, "reaction": {"type": "emoji", "emoji": "👍"}}]
        elif module_id == "multiple":
            actions = [{"type": "message.react", "message_ref": message_ref, "reaction": {"type": "emoji", "emoji": "👍"}}, {"type": "message.react", "message_ref": message_ref, "reaction": {"type": "emoji", "emoji": "👎"}}]
        else:
            actions = []
        response = {"protocol_version": 3, "type": "event_result", "request_id": request_id, "actions": actions}
    else:
        continue
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#;

    const V5_INVOKE_MODULE_PY: &str = r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    request_id = message["request_id"]
    if message["type"] == "initialize":
        response = {"protocol_version": 5, "type": "initialized", "request_id": request_id, "module_id": message["module_id"]}
        print(json.dumps(response), flush=True)
    elif message["type"] == "execute":
        invoke = {"protocol_version": 5, "type": "telegram.invoke", "request_id": request_id, "call_id": "call-1", "method": "account.updateStatus", "params": {"offline": True}}
        print(json.dumps(invoke), flush=True)
        result = json.loads(sys.stdin.readline())
        if result["type"] != "telegram.result" or result["request_id"] != request_id or result["call_id"] != "call-1":
            sys.exit(2)
        response = {"protocol_version": 5, "type": "result", "request_id": request_id, "text": json.dumps(result, sort_keys=True)}
        print(json.dumps(response), flush=True)
"#;

    const V5_DOUBLE_INVOKE_MODULE_PY: &str = r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    request_id = message["request_id"]
    if message["type"] == "initialize":
        print(json.dumps({"protocol_version": 5, "type": "initialized", "request_id": request_id, "module_id": message["module_id"]}), flush=True)
    elif message["type"] == "execute":
        for call_id in ["call-1", "call-2"]:
            print(json.dumps({"protocol_version": 5, "type": "telegram.invoke", "request_id": request_id, "call_id": call_id, "method": "account.updateStatus", "params": {"offline": True}}), flush=True)
"#;

    struct FakeGateway {
        result: Result<serde_json::Value, protocol::TelegramCallError>,
        contexts: Mutex<Vec<GatewayContext>>,
    }

    impl TelegramGateway for FakeGateway {
        fn invoke<'a>(
            &'a self,
            context: GatewayContext,
            _method: &'a str,
            _params: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<serde_json::Value, protocol::TelegramCallError>,
                    > + Send
                    + 'a,
            >,
        > {
            self.contexts.lock().unwrap().push(context);
            Box::pin(std::future::ready(self.result.clone()))
        }
    }

    struct HangingGateway;

    impl TelegramGateway for HangingGateway {
        fn invoke<'a>(
            &'a self,
            _context: GatewayContext,
            _method: &'a str,
            _params: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<serde_json::Value, protocol::TelegramCallError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn v5_nested_invoke_preserves_parent_and_success_envelope() {
        let (mut descriptor, directory) = create_fixture_module(V5_INVOKE_MODULE_PY, "v5-invoke");
        descriptor.protocol_version = 5;
        descriptor.capabilities = vec![ExternalCapability::TelegramAccountStatus];
        let gateway = Arc::new(FakeGateway {
            result: Ok(serde_json::Value::Bool(true)),
            contexts: Mutex::new(Vec::new()),
        });
        let mut process = ModuleProcess::start_with_gateway(descriptor, Some(gateway.clone()))
            .await
            .unwrap();
        let text = process.execute("run", "").await.unwrap();
        let envelope: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["result"], true);
        assert_eq!(gateway.contexts.lock().unwrap().len(), 1);
        assert_eq!(process.in_flight_request(), None);
        process.terminate().await;
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn v5_nested_invoke_returns_rpc_error_envelope() {
        let (mut descriptor, directory) = create_fixture_module(V5_INVOKE_MODULE_PY, "v5-rpc");
        descriptor.protocol_version = 5;
        descriptor.capabilities = vec![ExternalCapability::TelegramAccountStatus];
        let gateway = Arc::new(FakeGateway {
            result: Err(protocol::TelegramCallError {
                kind: "rpc",
                code: Some(420),
                name: Some("FLOOD_WAIT".to_owned()),
                message: "FLOOD_WAIT".to_owned(),
                retry_after_seconds: Some(9),
            }),
            contexts: Mutex::new(Vec::new()),
        });
        let mut process = ModuleProcess::start_with_gateway(descriptor, Some(gateway))
            .await
            .unwrap();
        let text = process.execute("run", "").await.unwrap();
        let envelope: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error"]["kind"], "rpc");
        assert_eq!(envelope["error"]["code"], 420);
        assert_eq!(envelope["error"]["name"], "FLOOD_WAIT");
        assert_eq!(envelope["error"]["retry_after_seconds"], 9);
        process.terminate().await;
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn v5_second_invoke_for_the_same_parent_fails_and_clears_state() {
        let (mut descriptor, directory) =
            create_fixture_module(V5_DOUBLE_INVOKE_MODULE_PY, "v5-double");
        descriptor.protocol_version = 5;
        descriptor.capabilities = vec![ExternalCapability::TelegramAccountStatus];
        let gateway = Arc::new(FakeGateway {
            result: Ok(serde_json::Value::Bool(true)),
            contexts: Mutex::new(Vec::new()),
        });
        let mut process = ModuleProcess::start_with_gateway(descriptor, Some(gateway))
            .await
            .unwrap();
        assert!(matches!(
            process.execute("run", "").await,
            Err(ExternalError::ProtocolDecode)
        ));
        assert_eq!(process.in_flight_request(), None);
        assert_eq!(process.status(), ProcessStatus::Crashed);
        process.terminate().await;
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn v5_hanging_gateway_receives_timeout_result_before_terminal_reply() {
        let (mut descriptor, directory) = create_fixture_module(V5_INVOKE_MODULE_PY, "v5-timeout");
        descriptor.protocol_version = 5;
        descriptor.capabilities = vec![ExternalCapability::TelegramAccountStatus];
        let mut process =
            ModuleProcess::start_with_gateway(descriptor, Some(Arc::new(HangingGateway)))
                .await
                .unwrap();
        let text = process.execute("run", "").await.unwrap();
        let envelope: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error"]["kind"], "timeout");
        assert_eq!(process.in_flight_request(), None);
        process.terminate().await;
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn v3_event_result_accepts_zero_ordinary_and_custom_emoji_actions() {
        for (id, expected) in [
            ("zero", vec![]),
            (
                "ordinary",
                vec![protocol::EventAction {
                    message_ref: "message-1".to_owned(),
                    reactions: vec![protocol::ReactionSpec::Emoji("👍".to_owned())],
                }],
            ),
            (
                "custom",
                vec![protocol::EventAction {
                    message_ref: "message-1".to_owned(),
                    reactions: vec![protocol::ReactionSpec::CustomEmoji {
                        document_id: "5456140674028019486".to_owned(),
                    }],
                }],
            ),
        ] {
            let (descriptor, directory) = create_v3_event_module(id);
            let entrypoint = descriptor.entrypoint.clone();
            make_script(
                &entrypoint,
                &V3_EVENT_MODULE_PY.replace("module_id = \"\"", &format!("module_id = {id:?}")),
            );
            let mut process = ModuleProcess::start(descriptor).await.unwrap();
            let (_, actions) = process
                .dispatch_event(MessageEventKind::Created, created_event())
                .await
                .unwrap();
            assert_eq!(actions, expected);
            process.terminate().await;
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn v3_event_failures_reject_the_protocol_and_clean_up_processes() {
        for (id, expected) in [
            ("wrong-id", ExternalError::WrongRequestId),
            ("malformed", ExternalError::ProtocolDecode),
            ("multiple", ExternalError::ProtocolDecode),
            ("exit", ExternalError::Unavailable),
            ("timeout", ExternalError::ExecutionTimeout),
        ] {
            let (descriptor, directory) = create_v3_event_module(id);
            let entrypoint = descriptor.entrypoint.clone();
            make_script(
                &entrypoint,
                &V3_EVENT_MODULE_PY.replace("module_id = \"\"", &format!("module_id = {id:?}")),
            );
            let mut process = ModuleProcess::start(descriptor).await.unwrap();
            let error = process
                .dispatch_event(MessageEventKind::Created, created_event())
                .await
                .unwrap_err();
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&expected)
            );
            assert_eq!(process.status(), ProcessStatus::Crashed);
            assert_eq!(process.in_flight_request(), None);
            assert_eq!(process.process_group_id, None);
            assert_eq!(process.crash_events.load(Ordering::Relaxed), 1);
            process.terminate().await;
            assert_eq!(process.process_group_id, None);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    const V2_LOG_MODULE_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        log = {"protocol_version": 2, "type": "log", "request_id": req_id, "level": "info", "message": "working"}
        sys.stdout.write(json.dumps(log) + "\n")
        sys.stdout.flush()
        resp = {"protocol_version": 2, "type": "result", "request_id": req_id, "text": "done"}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn module_log_with_matching_request_id_does_not_fail_the_request() {
        let (desc, dir) = create_fixture_module(V2_LOG_MODULE_PY, "v2-log");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.execute("run", "").await.unwrap();
        assert_eq!(result, "done");
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    const V2_WRONG_LOG_ID_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        log = {"protocol_version": 2, "type": "log", "request_id": "999", "level": "warn", "message": "stale"}
        sys.stdout.write(json.dumps(log) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn module_log_with_foreign_request_id_is_rejected() {
        let (desc, dir) = create_fixture_module(V2_WRONG_LOG_ID_PY, "v2-log-wrong-id");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.execute("run", "").await;
        assert!(matches!(result, Err(ExternalError::WrongRequestId)));
        assert_eq!(proc.status(), ProcessStatus::Crashed);
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    const V2_HANDSHAKE_LOG_MODULE_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        log = {"protocol_version": 2, "type": "log", "request_id": req_id, "level": "info", "message": "starting up"}
        sys.stdout.write(json.dumps(log) + "\n")
        sys.stdout.flush()
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn handshake_forwards_log_before_initialized() {
        let (desc, dir) = create_fixture_module(V2_HANDSHAKE_LOG_MODULE_PY, "v2-handshake-log");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        assert_eq!(proc.status(), ProcessStatus::Running);
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    const V2_HANDSHAKE_WRONG_LOG_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        log = {"protocol_version": 2, "type": "log", "request_id": "999", "level": "warn", "message": "stale"}
        sys.stdout.write(json.dumps(log) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn handshake_rejects_log_with_foreign_request_id() {
        let (desc, dir) =
            create_fixture_module(V2_HANDSHAKE_WRONG_LOG_PY, "v2-handshake-log-wrong");
        let result = ModuleProcess::start(desc).await;
        assert!(matches!(result, Err(ExternalError::WrongRequestId)));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// initialize OK, then execute replies with a terminal message of the wrong
    /// type (a `health` frame) carrying the matching request ID. The correlation
    /// state must survive until terminal validation, so the crash diagnostics
    /// carry the real execute request id.
    const V2_EXECUTE_WRONG_TERMINAL_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        sys.stderr.write("exec-id " + req_id + "\n")
        sys.stderr.flush()
        resp = {"protocol_version": 2, "type": "health", "request_id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn execute_wrong_terminal_type_preserves_request_id_in_crash_diagnostics() {
        let (desc, dir) = create_fixture_module(V2_EXECUTE_WRONG_TERMINAL_PY, "v2-exec-wrong-term");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.execute("run", "").await;
        assert!(matches!(result, Err(ExternalError::ProtocolDecode)));
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 1);
        let diagnostics = proc.last_crash_diagnostics.lock().unwrap().clone().unwrap();
        let echoed = last_echoed_id(&proc.snapshot_stderr(), "exec-id ");
        assert!(!echoed.is_empty());
        assert_eq!(diagnostics.request_id, echoed);
        assert_ne!(diagnostics.request_id, "-");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A protocol-valid application `error` frame with the matching request id
    /// is not a crash: it must not emit `external_module_crashed`.
    const V2_EXECUTE_ERROR_MODULE_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        resp = {"protocol_version": 2, "type": "error", "request_id": req_id, "code": "invalid_input", "message": "Name is required"}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn execute_application_error_is_not_a_crash() {
        let (desc, dir) = create_fixture_module(V2_EXECUTE_ERROR_MODULE_PY, "v2-exec-error");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.execute("run", "").await;
        assert!(matches!(result, Err(ExternalError::ModuleError)));
        assert_eq!(proc.status(), ProcessStatus::Running);
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 0);
        assert!(proc.last_crash_diagnostics.lock().unwrap().is_none());
        assert_eq!(proc.in_flight_request(), None);
        // The process is still Running; stop it explicitly instead of relying
        // on `kill_on_drop` for cleanup of a live module.
        proc.terminate().await;
        assert_eq!(proc.status(), ProcessStatus::Terminated);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The module replies with a correlated application error on the first
    /// execute and a normal result on every later execute. Proves the lifecycle
    /// contract: after an application error the same process must stay Running
    /// and serve the next request without a restart.
    const V2_EXECUTE_ERROR_THEN_RESULT_PY: &str = r#"#!/usr/bin/env python3
import sys, json
executions = 0
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        executions += 1
        if executions == 1:
            resp = {"protocol_version": 2, "type": "error", "request_id": req_id, "code": "invalid_input", "message": "Name is required"}
        else:
            resp = {"protocol_version": 2, "type": "result", "request_id": req_id, "text": "ok"}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn process_survives_application_error_and_serves_the_next_execute() {
        let (desc, dir) =
            create_fixture_module(V2_EXECUTE_ERROR_THEN_RESULT_PY, "v2-exec-error-then-result");
        let mut proc = ModuleProcess::start(desc).await.unwrap();

        let first = proc.execute("run", "").await;
        assert!(matches!(first, Err(ExternalError::ModuleError)));
        assert_eq!(proc.status(), ProcessStatus::Running);
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 0);
        assert!(proc.last_crash_diagnostics.lock().unwrap().is_none());
        assert_eq!(proc.in_flight_request(), None);

        // Same ModuleProcess, no restart: the module must still be usable.
        let second = proc.execute("run", "").await;
        assert!(matches!(second, Ok(ref text) if text == "ok"));
        assert_eq!(proc.status(), ProcessStatus::Running);
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 0);

        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    /// An application error frame with a foreign request id is a protocol
    /// failure: it must emit a crash event carrying the real execute request id.
    const V2_EXECUTE_FOREIGN_ERROR_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        sys.stderr.write("exec-id " + req_id + "\n")
        sys.stderr.flush()
        resp = {"protocol_version": 2, "type": "error", "request_id": "999", "code": "x", "message": "stale"}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn execute_foreign_error_id_is_a_crash_with_real_request_id() {
        let (desc, dir) =
            create_fixture_module(V2_EXECUTE_FOREIGN_ERROR_PY, "v2-exec-foreign-error");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.execute("run", "").await;
        assert!(matches!(result, Err(ExternalError::WrongRequestId)));
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 1);
        let diagnostics = proc.last_crash_diagnostics.lock().unwrap().clone().unwrap();
        let echoed = last_echoed_id(&proc.snapshot_stderr(), "exec-id ");
        assert!(!echoed.is_empty());
        assert_eq!(diagnostics.request_id, echoed);
        assert_ne!(diagnostics.request_id, "-");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A v3 event whose terminal reply is the wrong type (a `health` frame)
    /// with the matching request id must preserve the event request id in the
    /// crash diagnostics.
    const V3_EVENT_WRONG_TERMINAL_PY: &str = r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    request_id = message["request_id"]
    if message["type"] == "initialize":
        response = {"protocol_version": 3, "type": "initialized", "request_id": request_id, "module_id": message["module_id"]}
        print(json.dumps(response), flush=True)
    elif message["type"] == "event":
        sys.stderr.write("event-id " + request_id + "\n")
        sys.stderr.flush()
        response = {"protocol_version": 3, "type": "health", "request_id": request_id}
        print(json.dumps(response), flush=True)
"#;

    #[tokio::test]
    async fn event_wrong_terminal_type_preserves_request_id_in_crash_diagnostics() {
        let (mut descriptor, directory) =
            create_fixture_module(V3_EVENT_WRONG_TERMINAL_PY, "v3-event-wrong-term");
        descriptor.protocol_version = 3;
        let mut proc = ModuleProcess::start(descriptor).await.unwrap();
        let result = proc
            .dispatch_event(MessageEventKind::Created, created_event())
            .await;
        assert!(matches!(result, Err(ExternalError::ProtocolDecode)));
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 1);
        let diagnostics = proc.last_crash_diagnostics.lock().unwrap().clone().unwrap();
        let echoed = last_echoed_id(&proc.snapshot_stderr(), "event-id ");
        assert!(!echoed.is_empty());
        assert_eq!(diagnostics.request_id, echoed);
        assert_ne!(diagnostics.request_id, "-");
        fs::remove_dir_all(&directory).unwrap();
    }

    /// A handshake error frame with a foreign request id must be rejected as a
    /// correlation failure, not attributed to the wrong request.
    const V2_HANDSHAKE_FOREIGN_ERROR_PY: &str = r#"#!/usr/bin/env python3
import sys, json
initialized = False
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        if not initialized:
            initialized = True
            resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
            sys.stdout.write(json.dumps(resp) + "\n")
            sys.stdout.flush()
        else:
            sys.stderr.write("handshake-id " + req_id + "\n")
            sys.stderr.flush()
            resp = {"protocol_version": 2, "type": "error", "request_id": "999", "code": "x", "message": "stale"}
            sys.stdout.write(json.dumps(resp) + "\n")
            sys.stdout.flush()
"#;

    #[tokio::test]
    async fn handshake_foreign_error_id_is_rejected_with_real_request_id() {
        let (desc, dir) =
            create_fixture_module(V2_HANDSHAKE_FOREIGN_ERROR_PY, "handshake-foreign-error");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.handshake().await;
        assert!(matches!(result, Err(ExternalError::WrongRequestId)));
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 1);
        let diagnostics = proc.last_crash_diagnostics.lock().unwrap().clone().unwrap();
        let echoed = last_echoed_id(&proc.snapshot_stderr(), "handshake-id ");
        assert!(!echoed.is_empty());
        assert_eq!(diagnostics.request_id, echoed);
        assert_ne!(diagnostics.request_id, "-");
        fs::remove_dir_all(&dir).unwrap();
    }

    const STDERR_FAIL_MODULE_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        sys.stderr.write("diag: failing module marker\n")
        sys.stderr.flush()
        sys.stdout.write("this is not json\n")
        sys.stdout.flush()
        sys.exit(2)
"#;

    /// The module writes a marker to stderr, flushes it, then immediately sends
    /// a malformed stdout line and exits. The marker is already buffered in the
    /// kernel pipe when the crash path runs; `join_stderr_drain` must give the
    /// reader a bounded chance to consume it before aborting. This exercises the
    /// cleanup contract directly, without any test-only rendezvous.
    #[tokio::test]
    async fn stderr_pending_at_crash_is_drained_and_reported() {
        const MARKER: &str = "diag: failing module marker";
        let (desc, dir) = create_fixture_module(STDERR_FAIL_MODULE_PY, "stderr-fail");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.execute("run", "").await;
        assert!(matches!(result, Err(ExternalError::ProtocolDecode)));
        assert_eq!(proc.status(), ProcessStatus::Crashed);
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 1);
        let snapshot = proc.snapshot_stderr();
        assert!(snapshot.lossy_text().contains(MARKER));
        assert!(!snapshot.truncated);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn normal_termination_is_not_a_crash() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        proc.terminate().await;
        assert_eq!(proc.status(), ProcessStatus::Terminated);
        assert_eq!(proc.process_group_id, None);
        // Normal shutdown must never take the crash path.
        assert_eq!(proc.crash_events.load(Ordering::Relaxed), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Fails each lifecycle flow after echoing the request ID it received to
    /// stderr. The echo is the ground truth: the crash diagnostics must carry
    /// the same request ID that was actually sent for the failing request.
    const LIFECYCLE_FAILURE_MODULE_PY: &str = r#"#!/usr/bin/env python3
import sys, json, time
initialized = False
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        if not initialized:
            initialized = True
            resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
            sys.stdout.write(json.dumps(resp) + "\n")
            sys.stdout.flush()
        else:
            sys.stderr.write("lifecycle handshake " + req_id + "\n")
            sys.stderr.flush()
            resp = {"protocol_version": 2, "type": "error", "request_id": req_id, "code": "0", "message": "boom"}
            sys.stdout.write(json.dumps(resp) + "\n")
            sys.stdout.flush()
    elif msg_type == "health":
        sys.stderr.write("lifecycle health " + req_id + "\n")
        sys.stderr.flush()
        resp = {"protocol_version": 2, "type": "error", "request_id": req_id, "code": "0", "message": "boom"}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "shutdown":
        sys.stderr.write("lifecycle shutdown " + req_id + "\n")
        sys.stderr.flush()
        time.sleep(10)
"#;

    fn last_echoed_id(snapshot: &StderrCapture, prefix: &str) -> String {
        snapshot
            .lossy_text()
            .lines()
            .filter_map(|line| line.strip_prefix(prefix))
            .next_back()
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    fn lifecycle_failure_diagnostics(
        proc: &mut ModuleProcess,
        prefix: &str,
    ) -> (CrashDiagnostics, String) {
        let diagnostics = proc
            .last_crash_diagnostics
            .lock()
            .unwrap()
            .clone()
            .expect("crash path must record diagnostics");
        let echoed = last_echoed_id(&proc.snapshot_stderr(), prefix);
        assert!(!echoed.is_empty(), "module must have echoed its request id");
        (diagnostics, echoed)
    }

    #[tokio::test]
    async fn handshake_failure_diagnostics_carry_the_real_request_id() {
        let (desc, dir) = create_fixture_module(LIFECYCLE_FAILURE_MODULE_PY, "lifecycle");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.handshake().await;
        assert!(matches!(result, Err(ExternalError::ModuleError)));
        let (diagnostics, echoed) =
            lifecycle_failure_diagnostics(&mut proc, "lifecycle handshake ");
        assert_eq!(diagnostics.request_id, echoed);
        assert_ne!(diagnostics.request_id, "-");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn health_failure_diagnostics_carry_the_real_request_id() {
        let (desc, dir) = create_fixture_module(LIFECYCLE_FAILURE_MODULE_PY, "lifecycle");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.health_check().await;
        assert!(matches!(result, Err(ExternalError::ProtocolDecode)));
        let (diagnostics, echoed) = lifecycle_failure_diagnostics(&mut proc, "lifecycle health ");
        assert_eq!(diagnostics.request_id, echoed);
        assert_ne!(diagnostics.request_id, "-");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn shutdown_failure_diagnostics_carry_the_real_request_id() {
        let (desc, dir) = create_fixture_module(LIFECYCLE_FAILURE_MODULE_PY, "lifecycle");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.graceful_shutdown().await;
        assert!(matches!(result, Err(ExternalError::ShutdownTimeout)));
        let (diagnostics, echoed) = lifecycle_failure_diagnostics(&mut proc, "lifecycle shutdown ");
        assert_eq!(diagnostics.request_id, echoed);
        assert_ne!(diagnostics.request_id, "-");
        fs::remove_dir_all(&dir).unwrap();
    }
}
