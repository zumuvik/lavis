use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing;

use super::{
    MAX_COMMANDS_PER_MODULE,
    manifest::{ExternalCommandDescriptor, ExternalModuleDescriptor},
    process::{ModuleProcess, ProcessStatus},
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
    processes: BTreeMap<String, Arc<Mutex<ModuleProcess>>>,
    gateway: Option<Arc<dyn super::gateway::TelegramGateway>>,
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
        }
    }

    pub fn set_descriptors(&mut self, descriptors: Vec<ExternalModuleDescriptor>) {
        self.descriptors = descriptors;
    }

    pub fn set_gateway(&mut self, gateway: Arc<dyn super::gateway::TelegramGateway>) {
        self.gateway = Some(gateway);
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
        self.processes.get(id).is_some_and(|p| {
            p.try_lock()
                .is_ok_and(|p| p.status() == ProcessStatus::Running)
        })
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
                .and_then(|proc| proc.try_lock().ok())
            {
                match proc.status() {
                    ProcessStatus::Running => "активен",
                    ProcessStatus::Failed | ProcessStatus::Crashed => "ошибка",
                    ProcessStatus::Terminated => "остановлен",
                }
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
        let process = self.processes.get(module_id)?.try_lock().ok()?;
        (process.status() == ProcessStatus::Running)
            .then(|| process.descriptor().default_command.as_ref())
            .flatten()
            .map(|command| (module_id.to_owned(), command.clone()))
    }

    pub fn command_refs(&self) -> Vec<ExternalCommandRef> {
        let mut refs = Vec::new();
        for process in self.processes.values() {
            let Ok(process) = process.try_lock() else {
                continue;
            };
            if process.status() != ProcessStatus::Running {
                continue;
            }
            let desc = process.descriptor();
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
        let process = self.processes.get(module_id)?.try_lock().ok()?;
        if process.status() != ProcessStatus::Running {
            return None;
        }
        let cmd = process
            .descriptor()
            .commands
            .iter()
            .find(|c| c.name == command_name)?;
        Some(ExternalCommandRef {
            module_id: process.descriptor().id.clone(),
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

    pub async fn execute(
        &mut self,
        module_id: &str,
        command_name: &str,
        arguments: &str,
    ) -> Result<String, ExternalError> {
        let process = self
            .processes
            .get(module_id)
            .cloned()
            .ok_or(ExternalError::Unavailable)?;
        let mut process = process.lock().await;

        if process.status() != ProcessStatus::Running {
            return Err(ExternalError::Unavailable);
        }

        let result = process.execute(command_name, arguments).await?;
        Ok(result)
    }

    pub async fn dispatch_event(
        &mut self,
        module_id: &str,
        event: super::protocol::MessageEventKind,
        payload: super::protocol::MessageEvent,
    ) -> Result<(String, Vec<super::protocol::EventAction>), ExternalError> {
        let process = self
            .processes
            .get(module_id)
            .cloned()
            .ok_or(ExternalError::Unavailable)?;
        let mut process = process.lock().await;
        if process.status() != ProcessStatus::Running || process.descriptor().protocol_version < 3 {
            return Err(ExternalError::Unavailable);
        }
        process.dispatch_event(event, payload).await
    }

    pub async fn shutdown_all(&mut self) {
        tracing::info!(
            event = "external_modules_shutdown",
            "Shutting down external modules"
        );
        let processes: Vec<(String, Arc<Mutex<ModuleProcess>>)> = self
            .processes
            .iter()
            .map(|(id, process)| (id.clone(), process.clone()))
            .collect();
        for (id, process) in processes {
            let mut process = process.lock().await;
            match process.status() {
                ProcessStatus::Running => {
                    if process.graceful_shutdown().await.is_err() {
                        tracing::warn!(event = "external_module_shutdown_forced", module_id = %id, "Forcefully terminating external module");
                        process.terminate().await;
                    }
                }
                // A crashed process already ran fatal cleanup. Re-signalling
                // its old PID could hit a reused process group.
                ProcessStatus::Crashed | ProcessStatus::Failed | ProcessStatus::Terminated => {}
            }
        }
        self.processes.clear();
    }

    pub fn remove_crashed(&mut self, module_id: &str) {
        if let Some(proc) = self.processes.get(module_id)
            && proc
                .try_lock()
                .is_ok_and(|proc| proc.status() == ProcessStatus::Crashed)
        {
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
            .filter_map(|process| process.try_lock().ok())
            .filter(|process| process.status() == ProcessStatus::Running)
            .filter_map(|process| {
                process
                    .descriptor()
                    .default_command
                    .as_ref()
                    .map(|command| (process.descriptor().id.clone(), command.clone()))
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

    /// Starts children without retaining the manager mutex. Process I/O belongs
    /// to the individual process mutex; the manager only owns the index.
    pub async fn startup_enabled(&self, enabled_ids: &std::collections::BTreeSet<String>) {
        let (descriptors, gateway) = {
            let manager = self.inner.lock().await;
            (
                manager
                    .descriptors
                    .iter()
                    .filter(|descriptor| enabled_ids.contains(&descriptor.id))
                    .cloned()
                    .collect::<Vec<_>>(),
                manager.gateway.clone(),
            )
        };
        for descriptor in descriptors {
            let id = descriptor.id.clone();
            match ModuleProcess::start_with_gateway(descriptor.clone(), gateway.clone()).await {
                Ok(process) => {
                    let replaced = {
                        let mut manager = self.inner.lock().await;
                        manager
                            .processes
                            .insert(id.clone(), Arc::new(Mutex::new(process)))
                    };
                    if let Some(replaced) = replaced {
                        let mut replaced = replaced.lock().await;
                        replaced.terminate().await;
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
            let mut process = process.lock().await;
            if process.status() != ProcessStatus::Running {
                continue;
            }
            if process.graceful_shutdown().await.is_ok() {
                continue;
            }
            tracing::warn!(event = "external_module_shutdown_forced", module_id = %id, "Forcefully terminating external module");
            process.terminate().await;
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
        let mut process = process.lock().await;
        if process.status() != ProcessStatus::Running || process.descriptor().protocol_version < 3 {
            return Err(ExternalError::Unavailable);
        }
        process.dispatch_event(event, payload).await
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
        let mut process = process.lock().await;
        if process.status() != ProcessStatus::Running {
            return Err(ExternalError::Unavailable);
        }
        process
            .execute_with_entities(command_name, arguments, argument_entities)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::ExternalManager;
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
}
