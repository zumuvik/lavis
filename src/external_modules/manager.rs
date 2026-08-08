use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing;

use super::{
    MAX_COMMANDS_PER_MODULE,
    manifest::{ExternalCommandDescriptor, ExternalModuleDescriptor},
    process::{ModuleProcess, ProcessStatus},
    v6_executor::V6TelegramExecutor,
    v6_process::V6Process,
};
use crate::error::ExternalError;

#[derive(Debug, Clone)]
pub struct ExternalCommandRef {
    pub module_id: String,
    pub command_name: String,
    pub summary_ru: String,
    pub description_ru: String,
    pub usage: String,
    pub examples: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExternalModuleStatus {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub author: String,
    pub capabilities: Vec<String>,
    pub command_count: usize,
    pub status: &'static str,
}

pub struct ExternalManager {
    descriptors: Vec<ExternalModuleDescriptor>,
    processes: BTreeMap<String, ManagedProcess>,
    gateway: Option<Arc<dyn super::gateway::TelegramGateway>>,
    v6_executor: Option<Arc<dyn V6TelegramExecutor>>,
    /// Last known crash/startup-failure diagnostic per module, retained even
    /// after the process leaves the running index (startup failure, crash
    /// cleanup). A successful healthy start clears the entry.
    latest_diagnostics: BTreeMap<String, super::process::CrashDiagnostics>,
    /// Monotonic restart generation per module, incremented at every start
    /// attempt. Flows into crash diagnostics so restarts are distinguishable.
    restart_generations: BTreeMap<String, u64>,
}

#[derive(Clone)]
enum ManagedProcess {
    Legacy(Arc<Mutex<ModuleProcess>>),
    V6(V6Process),
}

#[derive(Debug, Eq, PartialEq)]
enum ProcessStartKind {
    Legacy,
    V6,
}

fn process_start_kind(
    protocol_version: u32,
    has_v6_executor: bool,
) -> Result<ProcessStartKind, ExternalError> {
    match protocol_version {
        2..=5 => Ok(ProcessStartKind::Legacy),
        6 if has_v6_executor => Ok(ProcessStartKind::V6),
        6 => Err(ExternalError::Unavailable),
        _ => Err(ExternalError::Unavailable),
    }
}

impl ManagedProcess {
    fn status(&self) -> Option<ProcessStatus> {
        match self {
            Self::Legacy(process) => process.try_lock().ok().map(|process| process.status()),
            Self::V6(process) => Some(process.status()),
        }
    }

    fn descriptor(&self) -> Option<ExternalModuleDescriptor> {
        match self {
            Self::Legacy(process) => process
                .try_lock()
                .ok()
                .map(|process| process.descriptor().clone()),
            Self::V6(process) => Some(process.descriptor().clone()),
        }
    }

    fn diagnostic_text(&self) -> Option<String> {
        match self {
            Self::Legacy(_) => None,
            Self::V6(process) => process
                .diagnostic()
                .map(|diagnostic| diagnostic.render_user()),
        }
    }
}

/// Wait for the v6 supervisor to record its crash diagnostic. The supervisor
/// builds the diagnostic during teardown, after the failed request reply, so a
/// synchronous read right after an `initialize`/`health` error can still see
/// `None`; poll briefly rather than dropping the process without the failure.
async fn retain_v6_diagnostic(process: &V6Process) -> Option<super::process::CrashDiagnostics> {
    for _ in 0..200 {
        if let Some(diagnostic) = process.diagnostic() {
            return Some(diagnostic);
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    None
}

async fn shutdown_process(id: &str, process: ManagedProcess) {
    match process {
        ManagedProcess::Legacy(process) => {
            let mut process = process.lock().await;
            if process.status() == ProcessStatus::Running
                && process.graceful_shutdown().await.is_err()
            {
                tracing::warn!(event = "external_module_shutdown_forced", module_id = %id, "Forcefully terminating external module");
                process.terminate().await;
            }
        }
        ManagedProcess::V6(process) => {
            if process.status() == ProcessStatus::Running
                && process.graceful_shutdown().await.is_err()
            {
                tracing::warn!(event = "external_module_shutdown_forced", module_id = %id, "Forcefully terminating external module");
                process.terminate().await;
            }
        }
    }
}

impl Default for ExternalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ExternalManager {
    pub fn new() -> Self {
        Self {
            descriptors: Vec::new(),
            processes: BTreeMap::new(),
            gateway: None,
            v6_executor: None,
            latest_diagnostics: BTreeMap::new(),
            restart_generations: BTreeMap::new(),
        }
    }

    pub fn set_descriptors(&mut self, descriptors: Vec<ExternalModuleDescriptor>) {
        self.descriptors = descriptors;
    }

    pub fn set_gateway(&mut self, gateway: Arc<dyn super::gateway::TelegramGateway>) {
        self.gateway = Some(gateway);
    }

    pub fn set_v6_executor(&mut self, executor: Arc<dyn V6TelegramExecutor>) {
        self.v6_executor = Some(executor);
    }

    pub fn descriptors(&self) -> &[ExternalModuleDescriptor] {
        &self.descriptors
    }

    /// Registers a newly installed descriptor without changing process or
    /// enabled-state ownership. A duplicate ID is rejected before mutation so
    /// a stale runtime snapshot cannot create ambiguous command routing.
    pub fn register_installed_descriptor(&mut self, descriptor: ExternalModuleDescriptor) -> bool {
        if self.descriptor_by_id(&descriptor.id).is_some() {
            return false;
        }
        self.descriptors.push(descriptor);
        true
    }

    pub fn descriptor_by_id(&self, id: &str) -> Option<&ExternalModuleDescriptor> {
        self.descriptors.iter().find(|d| d.id == id)
    }

    pub fn has_running_process(&self, id: &str) -> bool {
        self.processes.get(id).and_then(ManagedProcess::status) == Some(ProcessStatus::Running)
    }

    pub fn running_command_count(&self) -> usize {
        self.command_refs().len()
    }

    pub fn statuses(&self) -> Vec<ExternalModuleStatus> {
        let mut statuses = Vec::new();
        for desc in &self.descriptors {
            let status_label = if let Some(proc) = self
                .processes
                .get(&desc.id)
                .and_then(ManagedProcess::status)
            {
                match proc {
                    ProcessStatus::Running => "активен",
                    ProcessStatus::Failed | ProcessStatus::Crashed => "ошибка",
                    ProcessStatus::Terminated => "остановлен",
                }
            } else if self.latest_diagnostics.contains_key(&desc.id) {
                "ошибка"
            } else {
                "установлен, выключен"
            };
            statuses.push(ExternalModuleStatus {
                id: desc.id.clone(),
                display_name: desc.display_name.clone(),
                version: desc.version.clone(),
                author: desc.author.clone(),
                capabilities: desc
                    .capabilities
                    .iter()
                    .map(|c| c.as_str().to_owned())
                    .collect(),
                command_count: desc.commands.len(),
                status: status_label,
            });
        }
        statuses
    }

    /// Resolve a dotted command name `module-id.command-name` into
    /// `(module_id, command_name)` if the command exists on a running process.
    pub fn resolve_namespaced_command(&self, dotted: &str) -> Option<(String, String)> {
        let dot = dotted.find('.')?;
        let module_id = &dotted[..dot];
        let command_name = &dotted[dot + 1..];
        if module_id.is_empty() || command_name.is_empty() {
            return None;
        }
        let desc = self.descriptor_by_id(module_id)?;
        if !self.has_running_process(module_id) {
            return None;
        }
        desc.commands.iter().find(|c| c.name == command_name)?;
        Some((module_id.to_owned(), command_name.to_owned()))
    }

    pub fn resolve_default_command(&self, module_id: &str) -> Option<(String, String)> {
        let process = self.processes.get(module_id)?;
        (process.status() == Some(ProcessStatus::Running))
            .then(|| process.descriptor())
            .flatten()
            .and_then(|descriptor| {
                descriptor
                    .default_command
                    .map(|command| (module_id.to_owned(), command))
            })
    }

    pub fn command_refs(&self) -> Vec<ExternalCommandRef> {
        let mut refs = Vec::new();
        for process in self.processes.values() {
            if process.status() != Some(ProcessStatus::Running) {
                continue;
            }
            let Some(desc) = process.descriptor() else {
                continue;
            };
            for cmd in desc.commands.iter().take(MAX_COMMANDS_PER_MODULE) {
                refs.push(ExternalCommandRef {
                    module_id: desc.id.clone(),
                    command_name: cmd.name.clone(),
                    summary_ru: cmd.summary_ru.clone(),
                    description_ru: cmd.description_ru.clone(),
                    usage: cmd.usage.clone(),
                    examples: cmd.examples.clone(),
                });
            }
        }
        refs
    }

    pub fn find_command(&self, module_id: &str, command_name: &str) -> Option<ExternalCommandRef> {
        let process = self.processes.get(module_id)?;
        if process.status() != Some(ProcessStatus::Running) {
            return None;
        }
        let descriptor = process.descriptor()?;
        let cmd = descriptor
            .commands
            .iter()
            .find(|c| c.name == command_name)?;
        Some(ExternalCommandRef {
            module_id: descriptor.id.clone(),
            command_name: cmd.name.clone(),
            summary_ru: cmd.summary_ru.clone(),
            description_ru: cmd.description_ru.clone(),
            usage: cmd.usage.clone(),
            examples: cmd.examples.clone(),
        })
    }

    pub fn find_descriptor_command(
        &self,
        module_id: &str,
        command_name: &str,
    ) -> Option<&ExternalCommandDescriptor> {
        self.descriptor_by_id(module_id)?
            .commands
            .iter()
            .find(|c| c.name == command_name)
    }

    pub fn remove_crashed(&mut self, module_id: &str) {
        if let Some(proc) = self.processes.get(module_id)
            && proc.status() == Some(ProcessStatus::Crashed)
        {
            if let ManagedProcess::V6(process) = &self.processes[module_id]
                && let Some(diagnostic) = process.diagnostic()
            {
                self.latest_diagnostics
                    .insert(module_id.to_owned(), diagnostic);
            }
            self.processes.remove(module_id);
        }
    }

    pub fn has_command(&self, module_id: &str, command_name: &str) -> bool {
        self.find_command(module_id, command_name).is_some()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExternalRuntimeSnapshot {
    pub command_refs: Vec<ExternalCommandRef>,
    pub descriptors: Vec<ExternalModuleDescriptor>,
    pub module_statuses: Vec<ExternalModuleStatus>,
    pub active_commands: std::collections::HashSet<String>,
    pub active_defaults: std::collections::HashMap<String, String>,
}

impl ExternalRuntimeSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_manager(manager: &ExternalManager) -> Self {
        let command_refs = manager.command_refs();
        let descriptors = manager.descriptors().to_vec();
        let module_statuses = manager.statuses();
        let active_commands = command_refs
            .iter()
            .map(|r| format!("{}.{}", r.module_id, r.command_name))
            .collect();
        let active_defaults = manager
            .processes
            .values()
            .filter(|process| process.status() == Some(ProcessStatus::Running))
            .filter_map(|process| {
                let descriptor = process.descriptor()?;
                descriptor
                    .default_command
                    .map(|command| (descriptor.id, command))
            })
            .collect();
        Self {
            command_refs,
            descriptors,
            module_statuses,
            active_commands,
            active_defaults,
        }
    }

    pub fn refresh_from(&mut self, manager: &ExternalManager) {
        *self = Self::from_manager(manager);
    }
}

#[derive(Clone)]
pub struct ExternalManagerHandle {
    inner: Arc<Mutex<ExternalManager>>,
}

impl ExternalManagerHandle {
    pub fn new(manager: ExternalManager) -> Self {
        Self {
            inner: Arc::new(Mutex::new(manager)),
        }
    }

    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ExternalManager> {
        self.inner.lock().await
    }

    pub async fn snapshot(&self) -> ExternalRuntimeSnapshot {
        let mgr = self.inner.lock().await;
        ExternalRuntimeSnapshot::from_manager(&mgr)
    }

    pub async fn diagnostic_text(&self, module_id: &str) -> Option<String> {
        let mgr = self.inner.lock().await;
        match mgr
            .processes
            .get(module_id)
            .and_then(ManagedProcess::diagnostic_text)
        {
            Some(text) => Some(text),
            None => mgr
                .latest_diagnostics
                .get(module_id)
                .map(|diagnostic| diagnostic.render_user()),
        }
    }

    /// One-line crash summary (live process or retained), for `lm doctor`.
    pub async fn diagnostic_summary(&self, module_id: &str) -> Option<String> {
        let mgr = self.inner.lock().await;
        if let Some(ManagedProcess::V6(process)) = mgr.processes.get(module_id)
            && let Some(diagnostic) = process.diagnostic()
        {
            return Some(diagnostic.summary());
        }
        mgr.latest_diagnostics
            .get(module_id)
            .map(|diagnostic| diagnostic.summary())
    }

    /// Starts children without retaining the manager mutex. Process I/O belongs
    /// to the individual process mutex; the manager only owns the index.
    pub async fn startup_enabled(&self, enabled_ids: &std::collections::BTreeSet<String>) {
        let (descriptors, gateway, v6_executor) = {
            let manager = self.inner.lock().await;
            (
                manager
                    .descriptors
                    .iter()
                    .filter(|descriptor| enabled_ids.contains(&descriptor.id))
                    .cloned()
                    .collect::<Vec<_>>(),
                manager.gateway.clone(),
                manager.v6_executor.clone(),
            )
        };
        for descriptor in descriptors {
            let id = descriptor.id.clone();
            let process =
                match process_start_kind(descriptor.protocol_version, v6_executor.is_some()) {
                    Ok(ProcessStartKind::V6) => match v6_executor.clone() {
                        Some(executor) => {
                            let restart_generation = {
                                let mut manager = self.inner.lock().await;
                                let next =
                                    manager.restart_generations.get(&id).copied().unwrap_or(0) + 1;
                                manager.restart_generations.insert(id.clone(), next);
                                next
                            };
                            match V6Process::start(descriptor.clone(), executor, restart_generation)
                                .await
                            {
                                Ok(process) => {
                                    let handshake: Result<(), ExternalError> = match process
                                        .initialize(super::protocol::request_id(), id.clone())
                                        .await
                                    {
                                        Ok(super::protocol::V6InboundFrame::Initialized {
                                            module_id,
                                            ..
                                        }) if module_id == id => {
                                            match process
                                                .health(super::protocol::request_id())
                                                .await
                                            {
                                                Ok(super::protocol::V6InboundFrame::Health {
                                                    ..
                                                }) => Ok(()),
                                                Ok(_) => Err(ExternalError::ProtocolDecode),
                                                Err(error) => Err(error),
                                            }
                                        }
                                        Ok(_) => Err(ExternalError::ProtocolDecode),
                                        Err(error) => Err(error),
                                    };
                                    match handshake {
                                        Ok(()) => Ok(ManagedProcess::V6(process)),
                                        Err(error) => {
                                            // The supervisor records the crash
                                            // diagnostic during teardown, after
                                            // the failed request reply; poll
                                            // briefly so the failure is not
                                            // lost when the process is dropped.
                                            if let Some(diagnostic) =
                                                retain_v6_diagnostic(&process).await
                                            {
                                                let mut manager = self.inner.lock().await;
                                                manager
                                                    .latest_diagnostics
                                                    .insert(id.clone(), diagnostic);
                                            }
                                            process.terminate().await;
                                            Err(error)
                                        }
                                    }
                                }
                                Err(failure) => {
                                    let mut manager = self.inner.lock().await;
                                    manager
                                        .latest_diagnostics
                                        .insert(id.clone(), failure.diagnostics);
                                    Err(failure.error)
                                }
                            }
                        }
                        None => Err(ExternalError::Unavailable),
                    },
                    Ok(ProcessStartKind::Legacy) => {
                        ModuleProcess::start_with_gateway(descriptor.clone(), gateway.clone())
                            .await
                            .map(|process| ManagedProcess::Legacy(Arc::new(Mutex::new(process))))
                    }
                    Err(error) => Err(error),
                };
            match process {
                Ok(process) => {
                    // A healthy fresh start clears any retained crash history.
                    let replaced = {
                        let mut manager = self.inner.lock().await;
                        manager.latest_diagnostics.remove(&id);
                        manager.processes.insert(id.clone(), process)
                    };
                    if let Some(replaced) = replaced {
                        shutdown_process(&id, replaced).await;
                    }
                    tracing::info!(event = "external_module_started", module_id = %id, "External module started");
                }
                Err(error) => {
                    tracing::warn!(event = "external_module_startup_failed", module_id = %id, error = %error, "Не удалось запустить внешний модуль")
                }
            }
        }
    }

    /// Removes the index before awaiting child shutdown, so status refresh and
    /// routing never wait behind a slow process shutdown.
    pub async fn shutdown_all(&self) {
        let processes = {
            let mut manager = self.inner.lock().await;
            std::mem::take(&mut manager.processes)
        };
        for (id, process) in processes {
            shutdown_process(&id, process).await;
        }
    }

    pub async fn dispatch_event(
        &self,
        module_id: &str,
        event: super::protocol::MessageEventKind,
        payload: super::protocol::MessageEvent,
    ) -> Result<(String, Vec<super::protocol::EventAction>), ExternalError> {
        let process = {
            let manager = self.inner.lock().await;
            manager.processes.get(module_id).cloned()
        }
        .ok_or(ExternalError::Unavailable)?;
        match process {
            ManagedProcess::Legacy(process) => {
                let mut process = process.lock().await;
                if process.status() != ProcessStatus::Running
                    || process.descriptor().protocol_version < 3
                {
                    return Err(ExternalError::Unavailable);
                }
                process.dispatch_event(event, payload).await
            }
            ManagedProcess::V6(process) => process.dispatch_event_result(event, payload).await,
        }
    }

    pub async fn execute(
        &self,
        module_id: &str,
        command_name: &str,
        arguments: &str,
        argument_entities: &[super::protocol::CustomEmojiEntity],
    ) -> Result<String, ExternalError> {
        let process = {
            let manager = self.inner.lock().await;
            manager.processes.get(module_id).cloned()
        }
        .ok_or(ExternalError::Unavailable)?;
        match process {
            ManagedProcess::Legacy(process) => {
                let mut process = process.lock().await;
                if process.status() != ProcessStatus::Running {
                    return Err(ExternalError::Unavailable);
                }
                process
                    .execute_with_entities(command_name, arguments, argument_entities)
                    .await
            }
            ManagedProcess::V6(process) => {
                if process.status() != ProcessStatus::Running {
                    return Err(ExternalError::Unavailable);
                }
                process
                    .execute_command(command_name, arguments, argument_entities)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExternalManager, ProcessStartKind, process_start_kind};
    use crate::external_modules::manifest::{ExternalCommandDescriptor, ExternalModuleDescriptor};
    use std::path::PathBuf;

    fn descriptor(id: &str, version: &str) -> ExternalModuleDescriptor {
        ExternalModuleDescriptor {
            protocol_version: 2,
            id: id.to_owned(),
            display_name: "Sample".to_owned(),
            version: version.to_owned(),
            author: "Author".to_owned(),
            entrypoint: PathBuf::from("run"),
            module_dir: PathBuf::new(),
            capabilities: vec![],
            default_command: None,
            subscriptions: vec![],
            telegram_methods: vec![],
            actions: vec![],
            commands: vec![ExternalCommandDescriptor {
                name: "run".to_owned(),
                summary_ru: "run".to_owned(),
                description_ru: "run".to_owned(),
                usage: "run".to_owned(),
                examples: vec![],
            }],
        }
    }

    #[test]
    fn discovered_but_not_running_module_is_disabled_with_descriptor_command_count() {
        let mut manager = ExternalManager::new();
        manager.set_descriptors(vec![descriptor("sample", "1.0")]);

        let statuses = manager.statuses();
        assert_eq!(statuses[0].status, "установлен, выключен");
        assert_eq!(statuses[0].command_count, 1);
    }

    #[test]
    fn installed_descriptor_registration_rejects_duplicates_without_starting_a_process() {
        let mut manager = ExternalManager::new();
        assert!(manager.register_installed_descriptor(descriptor("sample", "1.0")));
        assert!(!manager.register_installed_descriptor(descriptor("sample", "2.0")));

        assert_eq!(manager.descriptors().len(), 1);
        assert_eq!(manager.descriptor_by_id("sample").unwrap().version, "1.0");
        assert!(!manager.has_running_process("sample"));
        assert!(manager.command_refs().is_empty());
    }

    #[test]
    fn schemas_two_through_five_select_the_legacy_process() {
        for version in 2..=5 {
            assert!(matches!(
                process_start_kind(version, false),
                Ok(ProcessStartKind::Legacy)
            ));
        }
    }

    #[test]
    fn schema_six_requires_the_v6_executor_and_selects_only_v6() {
        assert!(matches!(
            process_start_kind(6, false),
            Err(crate::error::ExternalError::Unavailable)
        ));
        assert!(matches!(
            process_start_kind(6, true),
            Ok(ProcessStartKind::V6)
        ));
    }

    #[tokio::test]
    async fn missing_v6_executor_does_not_publish_a_process() {
        let mut module = descriptor("sample", "1.0");
        module.protocol_version = 6;
        let manager = ExternalManager::new();
        let handle = super::ExternalManagerHandle::new(manager);
        {
            let mut manager = handle.lock().await;
            manager.set_descriptors(vec![module]);
        }

        handle
            .startup_enabled(&std::collections::BTreeSet::from(["sample".to_owned()]))
            .await;

        assert!(!handle.lock().await.has_running_process("sample"));
    }

    #[tokio::test]
    async fn startup_failure_is_retained_as_spawn_diagnostic_and_status_error() {
        struct NoopExecutor;
        impl super::super::v6_executor::V6TelegramExecutor for NoopExecutor {
            fn execute<'a>(
                &'a self,
                _context: super::super::v6_executor::V6ExecutionContext,
                _method: super::super::v6_registry::V6Method,
                _params: Box<serde_json::value::RawValue>,
            ) -> super::super::v6_executor::V6ExecutorFuture<'a> {
                Box::pin(async { Err(super::super::v6_executor::V6ExecutorError::Transport) })
            }
        }

        let root = std::env::temp_dir().join(format!(
            "lavis-manager-start-failure-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::external_modules::v6_process::set_test_state_base(root.join("state"));
        let mut module = descriptor("sample", "1.0");
        module.protocol_version = 6;
        module.module_dir = root.clone();
        module.entrypoint = root.join("missing");
        let handle = super::ExternalManagerHandle::new(ExternalManager::new());
        {
            let mut manager = handle.lock().await;
            manager.set_descriptors(vec![module]);
            manager.set_v6_executor(std::sync::Arc::new(NoopExecutor));
        }
        handle
            .startup_enabled(&std::collections::BTreeSet::from(["sample".to_owned()]))
            .await;
        let manager = handle.lock().await;
        assert_eq!(manager.statuses()[0].status, "ошибка");
        let diagnostic = manager
            .latest_diagnostics
            .get("sample")
            .expect("retained diagnostic");
        assert_eq!(diagnostic.lifecycle_stage, "spawn");
        assert_eq!(diagnostic.restart_generation, 1);
        drop(manager);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retained_diagnostic_marks_the_module_as_error_without_a_process() {
        let mut manager = ExternalManager::new();
        manager.set_descriptors(vec![descriptor("sample", "1.0")]);
        assert_eq!(manager.statuses()[0].status, "установлен, выключен");

        let diagnostic = super::super::process::CrashDiagnostics {
            module_id: "sample".to_owned(),
            protocol_version: 6,
            lifecycle_stage: "initialize".to_owned(),
            request_id: "-".to_owned(),
            error_category: "execution_timeout".to_owned(),
            error: "timed out".to_owned(),
            exit_code: None,
            signal: None,
            stderr_truncated: false,
            stderr: "-".to_owned(),
            timestamp_unix_ms: 1,
            restart_generation: 1,
        };
        manager
            .latest_diagnostics
            .insert("sample".to_owned(), diagnostic);
        assert_eq!(manager.statuses()[0].status, "ошибка");
    }

    #[test]
    fn remove_crashed_retains_the_last_diagnostic_before_removal() {
        let mut manager = ExternalManager::new();
        manager.set_descriptors(vec![descriptor("sample", "1.0")]);
        assert!(manager.latest_diagnostics.is_empty());

        // Only crashed v6 processes leave a retained diagnostic; a missing or
        // running process must not populate the retention map.
        manager.remove_crashed("sample");
        assert!(manager.latest_diagnostics.is_empty());
    }

    #[tokio::test]
    async fn handle_diagnostic_text_falls_back_to_retained_diagnostics() {
        let mut manager = ExternalManager::new();
        manager.set_descriptors(vec![descriptor("sample", "1.0")]);
        manager.latest_diagnostics.insert(
            "sample".to_owned(),
            super::super::process::CrashDiagnostics {
                module_id: "sample".to_owned(),
                protocol_version: 6,
                lifecycle_stage: "initialize".to_owned(),
                request_id: "-".to_owned(),
                error_category: "execution_timeout".to_owned(),
                error: "timed out".to_owned(),
                exit_code: None,
                signal: None,
                stderr_truncated: false,
                stderr: "-".to_owned(),
                timestamp_unix_ms: 1,
                restart_generation: 3,
            },
        );
        let handle = super::ExternalManagerHandle::new(manager);

        let text = handle.diagnostic_text("sample").await.expect("diagnostic");
        assert!(text.contains("stage=initialize"));
        assert!(text.contains("category=execution_timeout"));
        assert!(text.contains("generation=3"));
        assert_eq!(
            handle.diagnostic_summary("sample").await.as_deref(),
            Some("stage=initialize category=execution_timeout generation=3")
        );
        assert_eq!(handle.lock().await.statuses()[0].status, "ошибка");
    }
}
