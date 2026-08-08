//! Transport-independent control operations for installed external modules.
//!
//! This module deliberately exposes manifest metadata rather than filesystem
//! locations so CLI and Telegram adapters cannot accidentally disclose paths.

use super::{
    manifest::{
        ExternalAction, ExternalCapability, ExternalCommandDescriptor, ExternalModuleDescriptor,
        ExternalSubscription, validate_manifest_at, validate_module_id,
    },
    state::ExternalStateStore,
};
use std::{collections::BTreeSet, fs, io::Read, path::Path};
use thiserror::Error;

const MAX_DECLARATIVE_STATE_BYTES: usize = 4096;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ModuleControlError {
    #[error("invalid module ID")]
    InvalidModuleId,
    #[error("external module root is unavailable")]
    ModuleRootUnavailable,
    #[error("declarative module state is unreadable")]
    DeclarativeStateRead,
    #[error("declarative module state is too large")]
    DeclarativeStateTooLarge,
    #[error("declarative module state is malformed")]
    DeclarativeStateMalformed,
    #[error("module is not installed")]
    ModuleNotInstalled,
    #[error("installed module manifest is invalid")]
    InvalidInstalledModule,
    #[error("module is managed declaratively by NixOS")]
    DeclarativelyManaged,
    #[error("external module state operation failed")]
    State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleManagement {
    Manual,
    DeclarativeNixOs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfo {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub author: String,
    /// Validated relative entrypoint, never an absolute host path.
    pub entrypoint: String,
    pub protocol_version: u32,
    pub capabilities: Vec<ExternalCapability>,
    pub default_command: Option<String>,
    pub subscriptions: Vec<ExternalSubscription>,
    pub actions: Vec<ExternalAction>,
    pub commands: Vec<ExternalCommandDescriptor>,
    pub enabled: bool,
    pub management: ModuleManagement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleDiagnostic {
    InvalidModuleId,
    InvalidManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleListEntry {
    /// The directory name when it is valid Unicode. It is not a filesystem path.
    pub id: Option<String>,
    pub enabled: bool,
    pub management: ModuleManagement,
    pub module: Option<ModuleInfo>,
    pub diagnostic: Option<ModuleDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleList {
    pub modules: Vec<ModuleListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleOperation {
    pub module: ModuleInfo,
    /// Whether this operation changed the enabled state.
    pub changed: bool,
}

/// Lists immediate module directories. A broken module is represented by an
/// entry diagnostic and does not prevent valid neighbours from being listed.
pub fn list_modules(
    module_root: &Path,
    declarative_state_path: &Path,
    state: &ExternalStateStore,
) -> Result<ModuleList, ModuleControlError> {
    let declarative_ids = load_declarative_ids(declarative_state_path)?;
    if !module_root.exists() {
        return Ok(ModuleList {
            modules: Vec::new(),
        });
    }
    let entries =
        fs::read_dir(module_root).map_err(|_| ModuleControlError::ModuleRootUnavailable)?;
    let mut modules = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| ModuleControlError::ModuleRootUnavailable)?;
        let file_type = entry
            .file_type()
            .map_err(|_| ModuleControlError::ModuleRootUnavailable)?;
        if !file_type.is_dir() {
            continue;
        }
        let id = entry.file_name().to_str().map(str::to_owned);
        let management = if id.as_deref().is_some_and(|id| declarative_ids.contains(id)) {
            ModuleManagement::DeclarativeNixOs
        } else {
            ModuleManagement::Manual
        };
        let enabled = id.as_deref().is_some_and(|id| state.is_enabled(id));
        let module = match id.as_deref() {
            Some(id) if validate_module_id(id).is_ok() => {
                match validate_manifest_at(&entry.path().join("module.json"), Some(id)) {
                    Ok(descriptor) => Some(to_info(descriptor, enabled, management)),
                    Err(_) => None,
                }
            }
            _ => None,
        };
        let diagnostic = if module.is_some() {
            None
        } else if id
            .as_deref()
            .is_some_and(|id| validate_module_id(id).is_ok())
        {
            Some(ModuleDiagnostic::InvalidManifest)
        } else {
            Some(ModuleDiagnostic::InvalidModuleId)
        };
        modules.push(ModuleListEntry {
            id,
            enabled,
            management,
            module,
            diagnostic,
        });
    }
    modules.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(ModuleList { modules })
}

pub fn module_info(
    module_root: &Path,
    declarative_state_path: &Path,
    state: &ExternalStateStore,
    id: &str,
) -> Result<ModuleInfo, ModuleControlError> {
    validate_id(id)?;
    let management = management_for(id, &load_declarative_ids(declarative_state_path)?);
    installed_info(module_root, state, id, management)
}

pub async fn enable_module(
    module_root: &Path,
    declarative_state_path: &Path,
    state: &mut ExternalStateStore,
    id: &str,
) -> Result<ModuleOperation, ModuleControlError> {
    validate_id(id)?;
    let management = management_for(id, &load_declarative_ids(declarative_state_path)?);
    if management == ModuleManagement::DeclarativeNixOs {
        return Err(ModuleControlError::DeclarativelyManaged);
    }
    let mut module = installed_info(module_root, state, id, management)?;
    let changed = !state.is_enabled(id);
    state
        .enable(id)
        .await
        .map_err(|_| ModuleControlError::State)?;
    module.enabled = true;
    Ok(ModuleOperation { module, changed })
}

pub async fn disable_module(
    module_root: &Path,
    declarative_state_path: &Path,
    state: &mut ExternalStateStore,
    id: &str,
) -> Result<ModuleOperation, ModuleControlError> {
    validate_id(id)?;
    let management = management_for(id, &load_declarative_ids(declarative_state_path)?);
    if management == ModuleManagement::DeclarativeNixOs {
        return Err(ModuleControlError::DeclarativelyManaged);
    }
    let mut module = installed_info(module_root, state, id, management)?;
    let changed = state
        .disable_idempotent(id)
        .await
        .map_err(|_| ModuleControlError::State)?;
    module.enabled = false;
    Ok(ModuleOperation { module, changed })
}

fn validate_id(id: &str) -> Result<(), ModuleControlError> {
    validate_module_id(id).map_err(|_| ModuleControlError::InvalidModuleId)
}

fn installed_info(
    module_root: &Path,
    state: &ExternalStateStore,
    id: &str,
    management: ModuleManagement,
) -> Result<ModuleInfo, ModuleControlError> {
    let module_dir = module_root.join(id);
    if !module_dir.is_dir() {
        return Err(ModuleControlError::ModuleNotInstalled);
    }
    let descriptor = validate_manifest_at(&module_dir.join("module.json"), Some(id))
        .map_err(|_| ModuleControlError::InvalidInstalledModule)?;
    Ok(to_info(descriptor, state.is_enabled(id), management))
}

fn management_for(id: &str, declarative_ids: &BTreeSet<String>) -> ModuleManagement {
    if declarative_ids.contains(id) {
        ModuleManagement::DeclarativeNixOs
    } else {
        ModuleManagement::Manual
    }
}

fn to_info(
    descriptor: ExternalModuleDescriptor,
    enabled: bool,
    management: ModuleManagement,
) -> ModuleInfo {
    let entrypoint = descriptor
        .entrypoint
        .strip_prefix(&descriptor.module_dir)
        .ok()
        .and_then(|path| path.to_str())
        .filter(|path| !path.is_empty() && !path.starts_with('/') && !path.starts_with('\\'))
        .unwrap_or("<недоступно>")
        .to_owned();
    ModuleInfo {
        id: descriptor.id,
        display_name: descriptor.display_name,
        version: descriptor.version,
        author: descriptor.author,
        entrypoint,
        protocol_version: descriptor.protocol_version,
        capabilities: descriptor.capabilities,
        default_command: descriptor.default_command,
        subscriptions: descriptor.subscriptions,
        actions: descriptor.actions,
        commands: descriptor.commands,
        enabled,
        management,
    }
}

fn load_declarative_ids(path: &Path) -> Result<BTreeSet<String>, ModuleControlError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(_) => return Err(ModuleControlError::DeclarativeStateRead),
    };
    let mut bytes = Vec::new();
    file.take((MAX_DECLARATIVE_STATE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ModuleControlError::DeclarativeStateRead)?;
    if bytes.len() > MAX_DECLARATIVE_STATE_BYTES {
        return Err(ModuleControlError::DeclarativeStateTooLarge);
    }
    let ids: Vec<String> = serde_json::from_slice(&bytes)
        .map_err(|_| ModuleControlError::DeclarativeStateMalformed)?;
    let mut result = BTreeSet::new();
    for id in ids {
        validate_id(&id).map_err(|_| ModuleControlError::DeclarativeStateMalformed)?;
        if !result.insert(id) {
            return Err(ModuleControlError::DeclarativeStateMalformed);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_dir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("lavis-control-{nonce}-{seq}"));
        fs::create_dir_all(&path).unwrap();
        set_mode(&path, 0o700);
        path
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn install_module(root: &Path, id: &str) {
        let module = root.join(id);
        let bin = module.join("bin");
        fs::create_dir_all(&bin).unwrap();
        set_mode(&module, 0o700);
        set_mode(&bin, 0o700);
        let entrypoint = bin.join("module");
        fs::write(&entrypoint, "#!/bin/sh\nexit 0\n").unwrap();
        set_mode(&entrypoint, 0o700);
        fs::write(module.join("module.json"), format!(r#"{{"schema_version":2,"id":"{id}","name":"Echo","version":"1.0.0","author":"Test","entrypoint":"bin/module","commands":[{{"name":"echo","summary_ru":"Echo","description_ru":"Echo text","usage":"<text>"}}]}}"#)).unwrap();
        set_mode(&module.join("module.json"), 0o600);
    }

    async fn store(base: &Path) -> ExternalStateStore {
        ExternalStateStore::load(base.join("external-modules.json"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn no_modules_returns_an_empty_list() {
        let base = temp_dir();
        let state = store(&base).await;
        assert!(
            list_modules(
                &base.join("modules"),
                &base.join("declarative.json"),
                &state
            )
            .unwrap()
            .modules
            .is_empty()
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn lists_enabled_and_disabled_modules() {
        let base = temp_dir();
        let root = base.join("modules");
        fs::create_dir(&root).unwrap();
        set_mode(&root, 0o700);
        install_module(&root, "echo");
        install_module(&root, "other");
        let mut state = store(&base).await;
        state.enable("echo").await.unwrap();
        let list = list_modules(&root, &base.join("declarative.json"), &state).unwrap();
        assert!(
            list.modules
                .iter()
                .find(|m| m.id.as_deref() == Some("echo"))
                .unwrap()
                .enabled
        );
        assert!(
            !list
                .modules
                .iter()
                .find(|m| m.id.as_deref() == Some("other"))
                .unwrap()
                .enabled
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn corrupt_manifest_is_diagnosed_without_hiding_valid_modules() {
        let base = temp_dir();
        let root = base.join("modules");
        fs::create_dir(&root).unwrap();
        set_mode(&root, 0o700);
        install_module(&root, "echo");
        fs::create_dir(root.join("broken")).unwrap();
        set_mode(&root.join("broken"), 0o700);
        fs::write(root.join("broken/module.json"), "{").unwrap();
        set_mode(&root.join("broken/module.json"), 0o600);
        let state = store(&base).await;
        let list = list_modules(&root, &base.join("declarative.json"), &state).unwrap();
        assert!(list.modules.iter().any(|m| m.module.is_some()));
        assert_eq!(
            list.modules
                .iter()
                .find(|m| m.id.as_deref() == Some("broken"))
                .unwrap()
                .diagnostic,
            Some(ModuleDiagnostic::InvalidManifest)
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn enable_and_disable_are_idempotent_for_installed_modules() {
        let base = temp_dir();
        let root = base.join("modules");
        fs::create_dir(&root).unwrap();
        set_mode(&root, 0o700);
        install_module(&root, "echo");
        let mut state = store(&base).await;
        let declarative = base.join("declarative.json");
        assert!(
            enable_module(&root, &declarative, &mut state, "echo")
                .await
                .unwrap()
                .changed
        );
        assert!(
            !enable_module(&root, &declarative, &mut state, "echo")
                .await
                .unwrap()
                .changed
        );
        assert!(
            disable_module(&root, &declarative, &mut state, "echo")
                .await
                .unwrap()
                .changed
        );
        assert!(
            !disable_module(&root, &declarative, &mut state, "echo")
                .await
                .unwrap()
                .changed
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn missing_and_invalid_ids_are_rejected() {
        let base = temp_dir();
        let root = base.join("modules");
        fs::create_dir(&root).unwrap();
        set_mode(&root, 0o700);
        let mut state = store(&base).await;
        let declarative = base.join("declarative.json");
        assert_eq!(
            enable_module(&root, &declarative, &mut state, "missing")
                .await
                .unwrap_err(),
            ModuleControlError::ModuleNotInstalled
        );
        assert_eq!(
            enable_module(&root, &declarative, &mut state, "BAD")
                .await
                .unwrap_err(),
            ModuleControlError::InvalidModuleId
        );
        assert_eq!(
            disable_module(&root, &declarative, &mut state, "missing")
                .await
                .unwrap_err(),
            ModuleControlError::ModuleNotInstalled
        );
        assert_eq!(
            disable_module(&root, &declarative, &mut state, "BAD")
                .await
                .unwrap_err(),
            ModuleControlError::InvalidModuleId
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn declarative_modules_are_rejected_for_mutation() {
        let base = temp_dir();
        let root = base.join("modules");
        fs::create_dir(&root).unwrap();
        set_mode(&root, 0o700);
        install_module(&root, "echo");
        let declarative = base.join("declarative.json");
        fs::write(&declarative, "[\"echo\"]").unwrap();
        let mut state = store(&base).await;
        assert_eq!(
            enable_module(&root, &declarative, &mut state, "echo")
                .await
                .unwrap_err(),
            ModuleControlError::DeclarativelyManaged
        );
        assert_eq!(
            disable_module(&root, &declarative, &mut state, "echo")
                .await
                .unwrap_err(),
            ModuleControlError::DeclarativelyManaged
        );
        assert_eq!(
            module_info(&root, &declarative, &state, "echo")
                .unwrap()
                .management,
            ModuleManagement::DeclarativeNixOs
        );
        fs::remove_dir_all(base).unwrap();
    }
}
