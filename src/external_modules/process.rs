//! Shared supervision helpers for external module processes: status,
//! bounded stderr capture, and crash diagnostics. The v6 supervisor in
//! `v6_process` owns the transport; only the pieces both it and the manager
//! need live here.

use super::manifest::ExternalModuleDescriptor;
use crate::error::ExternalError;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Child, time::timeout};

pub const MAX_STDERR_CAPTURE: usize = 16 * 1024;
/// How long to wait for the stderr reader to drain bytes already buffered in
/// the kernel pipe before aborting it. Bounded so cleanup never blocks on a
/// descendant that retains the inherited stderr FD.
pub const STDERR_DRAIN_GRACE: Duration = Duration::from_millis(100);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Failed,
    Crashed,
    Terminated,
}

#[derive(Debug, Clone)]
pub(crate) struct LegacyMetadataView {
    pub(crate) descriptor: Arc<ExternalModuleDescriptor>,
    pub(crate) status: ProcessStatus,
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

/// Supervisor-supplied detail for a crash diagnostic: which lifecycle stage
/// failed and how the process exited, so the rendered diagnostic says more
/// than "the module stopped".
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

#[cfg(not(unix))]
async fn secure_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

pub async fn reap_child(mut child: Child) {
    let _ = child.wait().await;
}
