use crate::error::ExternalError;
use serde::Deserialize;
use std::{
    fs::{self, File, symlink_metadata},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_COMMANDS: usize = 32;
const MAX_EXAMPLE_COUNT: usize = 16;
const MAX_EXAMPLE_CHARS: usize = 256;
const MAX_SUMMARY_CHARS: usize = 120;
const MAX_DESCRIPTION_CHARS: usize = 2000;
const MAX_NAME_CHARS: usize = 64;
const MAX_VERSION_CHARS: usize = 32;
const MAX_AUTHOR_CHARS: usize = 128;
const MAX_USAGE_CHARS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalModuleDescriptor {
    pub protocol_version: u32,
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub author: String,
    pub entrypoint: PathBuf,
    pub module_dir: PathBuf,
    pub capabilities: Vec<ExternalCapability>,
    pub default_command: Option<String>,
    pub subscriptions: Vec<ExternalSubscription>,
    pub actions: Vec<ExternalAction>,
    pub commands: Vec<ExternalCommandDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCommandDescriptor {
    pub name: String,
    pub summary_ru: String,
    pub description_ru: String,
    pub usage: String,
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExternalCapability {
    HostInformation,
    Network,
    PersistentStateRead,
    PersistentStateWrite,
    MessageRead,
    MessagePeerId,
    MessageReact,
}

impl ExternalCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HostInformation => "host_information",
            Self::Network => "network",
            Self::PersistentStateRead => "persistent_state_read",
            Self::PersistentStateWrite => "persistent_state_write",
            Self::MessageRead => "message.read",
            Self::MessagePeerId => "message.peer_id",
            Self::MessageReact => "message.react",
        }
    }

    pub fn description_ru(self) -> &'static str {
        match self {
            Self::HostInformation => "сведения о хосте",
            Self::Network => "сеть",
            Self::PersistentStateRead => "чтение постоянного состояния",
            Self::PersistentStateWrite => "изменение постоянного состояния",
            Self::MessageRead => "чтение сообщений",
            Self::MessagePeerId => "идентификатор чата сообщения",
            Self::MessageReact => "реакции на сообщения",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "host_information" => Some(Self::HostInformation),
            "network" => Some(Self::Network),
            "persistent_state_read" => Some(Self::PersistentStateRead),
            "persistent_state_write" => Some(Self::PersistentStateWrite),
            "message.read" => Some(Self::MessageRead),
            "message.peer_id" => Some(Self::MessagePeerId),
            "message.react" => Some(Self::MessageReact),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExternalSubscription {
    MessageCreated,
    MessageEdited,
}
impl ExternalSubscription {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MessageCreated => "message.created",
            Self::MessageEdited => "message.edited",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "message.created" => Some(Self::MessageCreated),
            "message.edited" => Some(Self::MessageEdited),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExternalAction {
    MessageReact,
}
impl ExternalAction {
    fn from_str(value: &str) -> Option<Self> {
        (value == "message.react").then_some(Self::MessageReact)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    schema_version: u32,
    id: String,
    name: String,
    version: String,
    author: String,
    entrypoint: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    commands: Vec<ManifestCommand>,
    #[serde(default)]
    default_command: Option<String>,
    #[serde(default)]
    subscriptions: Vec<String>,
    #[serde(default)]
    actions: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestCommand {
    name: String,
    summary_ru: String,
    description_ru: String,
    usage: String,
    #[serde(default)]
    examples: Vec<String>,
}

pub fn validate_module_id(id: &str) -> Result<(), ExternalError> {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return Err(ExternalError::InvalidModuleId);
    }
    if !bytes[0].is_ascii_lowercase() {
        return Err(ExternalError::InvalidModuleId);
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err(ExternalError::InvalidModuleId);
    }
    Ok(())
}

pub fn validate_command_name(name: &str) -> Result<(), ExternalError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return Err(ExternalError::InvalidCommandName);
    }
    if !bytes[0].is_ascii_lowercase() {
        return Err(ExternalError::InvalidCommandName);
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err(ExternalError::InvalidCommandName);
    }
    Ok(())
}

/// Validate display text shared by manifest and source identity boundaries.
pub(crate) fn validate_display_single_line(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(|c| c.is_control() || is_bidi_control(c))
}

fn validate_single_line(value: &str) -> bool {
    validate_display_single_line(value)
}

fn usage_is_argument_syntax(usage: &str, command_name: &str) -> bool {
    usage
        .split_whitespace()
        .next()
        .is_none_or(|first| first != command_name)
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

fn validate_entrypoint(entrypoint: &str) -> Result<(), ExternalError> {
    if entrypoint.is_empty() {
        return Err(ExternalError::InvalidEntrypoint);
    }
    if entrypoint.contains("..") {
        return Err(ExternalError::PathEscape);
    }
    if entrypoint.starts_with('/') || entrypoint.starts_with('\\') {
        return Err(ExternalError::InvalidEntrypoint);
    }
    if entrypoint
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("file:"))
    {
        return Err(ExternalError::InvalidEntrypoint);
    }
    Ok(())
}

fn is_world_writable(mode: u32) -> bool {
    mode & 0o002 != 0
}

fn is_group_writable(mode: u32) -> bool {
    mode & 0o020 != 0
}

fn is_regular_file(path: &Path) -> Result<bool, ExternalError> {
    let meta = symlink_metadata(path).map_err(|_| ExternalError::NotReadable)?;
    if meta.file_type().is_symlink() {
        return Err(ExternalError::SymlinkEscape);
    }
    if !meta.file_type().is_file() {
        return Err(ExternalError::NotReadable);
    }
    #[cfg(unix)]
    {
        if is_group_writable(meta.mode()) || is_world_writable(meta.mode()) {
            return Err(ExternalError::UnsafePermissions);
        }
    }
    Ok(true)
}

fn is_regular_directory(path: &Path) -> Result<(), ExternalError> {
    let meta = symlink_metadata(path).map_err(|_| ExternalError::NotReadable)?;
    if meta.file_type().is_symlink() {
        return Err(ExternalError::SymlinkEscape);
    }
    if !meta.file_type().is_dir() {
        return Err(ExternalError::NotReadable);
    }
    #[cfg(unix)]
    {
        if is_group_writable(meta.mode()) || is_world_writable(meta.mode()) {
            return Err(ExternalError::UnsafePermissions);
        }
    }
    Ok(())
}

pub fn validate_manifest_at(
    path: &Path,
    expected_id: Option<&str>,
) -> Result<ExternalModuleDescriptor, ExternalError> {
    let is_executable = |p: &Path| {
        #[cfg(unix)]
        {
            p.metadata()
                .ok()
                .is_some_and(|m| m.is_file() && m.mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            p.metadata().ok().map_or(false, |m| m.is_file())
        }
    };

    let parent = path.parent().ok_or(ExternalError::NotReadable)?;
    is_regular_directory(parent)?;

    let meta = symlink_metadata(path).map_err(|_| ExternalError::NotReadable)?;
    if meta.file_type().is_symlink() {
        return Err(ExternalError::SymlinkEscape);
    }
    if !meta.file_type().is_file() {
        return Err(ExternalError::NotReadable);
    }
    #[cfg(unix)]
    {
        if is_group_writable(meta.mode()) || is_world_writable(meta.mode()) {
            return Err(ExternalError::UnsafePermissions);
        }
    }

    if meta.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(ExternalError::ManifestTooLarge);
    }

    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| ExternalError::NotReadable)?
        .take((MAX_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ExternalError::NotReadable)?;

    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ExternalError::ManifestTooLarge);
    }

    let manifest: ManifestFile =
        serde_json::from_slice(&bytes).map_err(|_| ExternalError::MalformedManifest)?;

    if !matches!(manifest.schema_version, 2..=4) {
        return Err(ExternalError::UnsupportedSchemaVersion);
    }

    validate_module_id(&manifest.id)?;

    if let Some(eid) = expected_id
        && manifest.id != eid
    {
        return Err(ExternalError::IdMismatch);
    }

    if !validate_single_line(&manifest.name) || manifest.name.chars().count() > MAX_NAME_CHARS {
        return Err(ExternalError::InvalidMetadata);
    }
    if !validate_single_line(&manifest.version)
        || manifest.version.chars().count() > MAX_VERSION_CHARS
    {
        return Err(ExternalError::InvalidMetadata);
    }
    if !validate_single_line(&manifest.author) || manifest.author.chars().count() > MAX_AUTHOR_CHARS
    {
        return Err(ExternalError::InvalidMetadata);
    }

    validate_entrypoint(&manifest.entrypoint)?;

    let entrypoint_path = parent.join(&manifest.entrypoint);
    let canonical_entrypoint =
        std::fs::canonicalize(&entrypoint_path).map_err(|_| ExternalError::InvalidEntrypoint)?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(|_| ExternalError::NotReadable)?;
    if !canonical_entrypoint.starts_with(&canonical_parent) {
        return Err(ExternalError::PathEscape);
    }
    is_regular_file(&entrypoint_path)?;
    if !is_executable(&entrypoint_path) {
        return Err(ExternalError::NotReadable);
    }

    let mut seen_capabilities = Vec::new();
    for cap_str in &manifest.capabilities {
        let cap = ExternalCapability::from_str(cap_str).ok_or(ExternalError::InvalidCapability)?;
        if seen_capabilities.contains(&cap) {
            return Err(ExternalError::InvalidCapability);
        }
        seen_capabilities.push(cap);
    }

    let mut subscriptions = Vec::new();
    let mut actions = Vec::new();
    if manifest.schema_version == 2
        && (manifest.default_command.is_some()
            || !manifest.subscriptions.is_empty()
            || !manifest.actions.is_empty())
    {
        return Err(ExternalError::UnsupportedSchemaVersion);
    }
    for value in &manifest.subscriptions {
        let subscription =
            ExternalSubscription::from_str(value).ok_or(ExternalError::InvalidArgument)?;
        if subscription == ExternalSubscription::MessageEdited && manifest.schema_version < 4 {
            return Err(ExternalError::UnsupportedSchemaVersion);
        }
        if subscriptions.contains(&subscription) {
            return Err(ExternalError::InvalidArgument);
        }
        subscriptions.push(subscription);
    }
    for value in &manifest.actions {
        let action = ExternalAction::from_str(value).ok_or(ExternalError::InvalidArgument)?;
        if actions.contains(&action) {
            return Err(ExternalError::InvalidArgument);
        }
        actions.push(action);
    }

    if manifest.commands.is_empty() {
        return Err(ExternalError::InvalidCommandCount);
    }
    if manifest.commands.len() > MAX_COMMANDS {
        return Err(ExternalError::InvalidCommandCount);
    }

    let mut seen_names = Vec::new();
    let mut commands = Vec::new();
    for mc in &manifest.commands {
        validate_command_name(&mc.name)?;
        if seen_names.contains(&mc.name) {
            return Err(ExternalError::DuplicateCommand);
        }
        seen_names.push(mc.name.clone());

        if !validate_single_line(&mc.summary_ru)
            || mc.summary_ru.chars().count() > MAX_SUMMARY_CHARS
        {
            return Err(ExternalError::InvalidMetadata);
        }
        if mc.description_ru.is_empty() || mc.description_ru.chars().count() > MAX_DESCRIPTION_CHARS
        {
            return Err(ExternalError::InvalidMetadata);
        }
        if mc
            .description_ru
            .chars()
            .any(|c| (c.is_control() && c != '\n') || is_bidi_control(c))
        {
            return Err(ExternalError::InvalidMetadata);
        }
        if !validate_single_line(&mc.usage)
            || mc.usage.chars().count() > MAX_USAGE_CHARS
            || !usage_is_argument_syntax(&mc.usage, &mc.name)
        {
            return Err(ExternalError::InvalidMetadata);
        }

        if mc.examples.len() > MAX_EXAMPLE_COUNT {
            return Err(ExternalError::InvalidMetadata);
        }
        for ex in &mc.examples {
            if !validate_single_line(ex) || ex.chars().count() > MAX_EXAMPLE_CHARS {
                return Err(ExternalError::InvalidMetadata);
            }
        }

        commands.push(ExternalCommandDescriptor {
            name: mc.name.clone(),
            summary_ru: mc.summary_ru.clone(),
            description_ru: mc.description_ru.clone(),
            usage: mc.usage.clone(),
            examples: mc.examples.clone(),
        });
    }

    if let Some(default_command) = &manifest.default_command {
        validate_command_name(default_command)?;
        if !commands
            .iter()
            .any(|command| command.name == *default_command)
        {
            return Err(ExternalError::InvalidArgument);
        }
    }
    if !subscriptions.is_empty() && !seen_capabilities.contains(&ExternalCapability::MessageRead) {
        return Err(ExternalError::InvalidCapability);
    }
    if seen_capabilities.contains(&ExternalCapability::MessagePeerId)
        && !seen_capabilities.contains(&ExternalCapability::MessageRead)
    {
        return Err(ExternalError::InvalidCapability);
    }
    if actions.contains(&ExternalAction::MessageReact)
        && !seen_capabilities.contains(&ExternalCapability::MessageReact)
    {
        return Err(ExternalError::InvalidCapability);
    }

    Ok(ExternalModuleDescriptor {
        protocol_version: manifest.schema_version,
        id: manifest.id,
        display_name: manifest.name,
        version: manifest.version,
        author: manifest.author,
        entrypoint: entrypoint_path,
        module_dir: canonical_parent,
        capabilities: seen_capabilities,
        default_command: manifest.default_command,
        subscriptions,
        actions,
        commands,
    })
}

pub fn discover_modules(root: &Path) -> Result<Vec<ExternalModuleDescriptor>, ExternalError> {
    let mut descriptors = Vec::new();
    if !root.exists() {
        return Ok(descriptors);
    }
    is_regular_directory(root)?;

    for entry in fs::read_dir(root).map_err(|_| ExternalError::NotReadable)? {
        let entry = entry.map_err(|_| ExternalError::NotReadable)?;
        let dir_path = entry.path();
        if !dir_path.is_dir() {
            continue;
        }
        let dir_name = dir_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_owned())
            .ok_or(ExternalError::InvalidModuleId)?;

        if validate_module_id(&dir_name).is_err() {
            continue;
        }

        let manifest_path = dir_path.join("module.json");
        match validate_manifest_at(&manifest_path, Some(&dir_name)) {
            Ok(desc) => descriptors.push(desc),
            Err(_) => continue,
        }
    }

    Ok(descriptors)
}

#[cfg(all(test, feature = "fixture-tests"))]
mod tests {
    use super::*;

    fn write_manifest(dir: &Path, json: &[u8]) -> PathBuf {
        let path = dir.join("module.json");
        fs::write(&path, json).unwrap();
        set_permissions(dir, 0o700);
        set_permissions(&path, 0o600);
        path
    }

    fn set_permissions(path: &Path, mode: u32) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
        }
    }

    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = fs::metadata(path).unwrap();
            let mode = meta.permissions().mode() | 0o111;
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    fn temp_dir() -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("lavis-manifest-test-{nonce}"));
        fs::create_dir_all(&dir).unwrap();
        set_permissions(&dir, 0o700);
        dir
    }

    fn valid_manifest_json() -> Vec<u8> {
        br#"{
            "schema_version": 2,
            "id": "echo",
            "name": "Echo",
            "version": "0.1.0",
            "author": "Example author",
            "entrypoint": "bin/echo-module",
            "capabilities": [],
            "commands": [
                {
                    "name": "repeat",
                    "summary_ru": "Repeat text",
                    "description_ru": "Returns the given text back.",
                    "usage": "<text>",
                    "examples": ["Hello"]
                }
            ]
        }"#
        .to_vec()
    }

    fn create_module_dir(base: &Path, id: &str) -> PathBuf {
        let dir = base.join(id);
        fs::create_dir_all(&dir).unwrap();
        set_permissions(&dir, 0o700);
        let bindir = dir.join("bin");
        fs::create_dir_all(&bindir).unwrap();
        set_permissions(&bindir, 0o700);
        let entry = bindir.join("echo-module");
        fs::write(&entry, "#!/bin/sh\necho \"$@\"").unwrap();
        make_executable(&entry);
        dir
    }

    #[test]
    fn valid_manifest_passes() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let path = write_manifest(&dir, &valid_manifest_json());
        let desc = validate_manifest_at(&path, Some("echo")).unwrap();
        assert_eq!(desc.id, "echo");
        assert_eq!(desc.display_name, "Echo");
        assert_eq!(desc.version, "0.1.0");
        assert_eq!(desc.author, "Example author");
        assert_eq!(desc.commands.len(), 1);
        assert_eq!(desc.commands[0].name, "repeat");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn unknown_field_rejected() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let json = br#"{
            "schema_version": 2,
            "id": "echo",
            "name": "Echo",
            "version": "0.1.0",
            "author": "Example author",
            "entrypoint": "bin/echo-module",
            "unknown_field": true,
            "capabilities": [],
            "commands": [
                { "name": "repeat", "summary_ru": "Test", "description_ru": "Test desc", "usage": "<t>", "examples": [] }
            ]
        }"#;
        write_manifest(&dir, json);
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::MalformedManifest)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn wrong_schema_version_rejected() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["schema_version"] = serde_json::json!(1);
        write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::UnsupportedSchemaVersion)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn v3_manifest_requires_declared_permissions_and_default_target() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["schema_version"] = serde_json::json!(3);
        json["default_command"] = serde_json::json!("repeat");
        json["subscriptions"] = serde_json::json!(["message.created"]);
        json["actions"] = serde_json::json!(["message.react"]);
        json["capabilities"] = serde_json::json!(["message.read", "message.react"]);
        let path = write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        let descriptor = validate_manifest_at(&path, Some("echo")).unwrap();
        assert_eq!(descriptor.protocol_version, 3);
        assert_eq!(descriptor.default_command.as_deref(), Some("repeat"));

        json["capabilities"] = serde_json::json!([]);
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::InvalidCapability)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn v4_manifest_accepts_edited_message_subscription() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["schema_version"] = serde_json::json!(4);
        json["default_command"] = serde_json::json!("repeat");
        json["subscriptions"] = serde_json::json!(["message.created", "message.edited"]);
        json["actions"] = serde_json::json!(["message.react"]);
        json["capabilities"] = serde_json::json!(["message.read", "message.react"]);
        let path = write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        let descriptor = validate_manifest_at(&path, Some("echo")).unwrap();
        assert_eq!(descriptor.protocol_version, 4);
        assert!(
            descriptor
                .subscriptions
                .contains(&ExternalSubscription::MessageEdited)
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn peer_id_capability_requires_message_read() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["schema_version"] = serde_json::json!(4);
        json["capabilities"] = serde_json::json!(["message.read", "message.peer_id"]);
        let path = write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        let descriptor = validate_manifest_at(&path, Some("echo")).unwrap();
        assert!(
            descriptor
                .capabilities
                .contains(&ExternalCapability::MessagePeerId)
        );

        json["capabilities"] = serde_json::json!(["message.peer_id"]);
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::InvalidCapability)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn v3_manifest_rejects_edited_message_subscription() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["schema_version"] = serde_json::json!(3);
        json["subscriptions"] = serde_json::json!(["message.edited"]);
        json["capabilities"] = serde_json::json!(["message.read"]);
        let path = write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::UnsupportedSchemaVersion)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn v2_manifest_rejects_a_default_command() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["default_command"] = serde_json::json!("repeat");
        let path = write_manifest(&dir, &serde_json::to_vec(&json).unwrap());

        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::UnsupportedSchemaVersion)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn oversized_manifest_rejected() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let oversized = vec![b'x'; MAX_MANIFEST_BYTES + 1];
        write_manifest(&dir, &oversized);
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::ManifestTooLarge)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn invalid_id_rejected() {
        assert!(validate_module_id("").is_err());
        assert!(validate_module_id("UPPERCASE").is_err());
        assert!(validate_module_id("0start").is_err());
        assert!(validate_module_id("a".repeat(33).as_str()).is_err());
        assert!(validate_module_id("has space").is_err());
        assert!(validate_module_id("valid-id-123").is_ok());
        assert!(validate_module_id("a").is_ok());
    }

    #[test]
    fn invalid_command_name_rejected() {
        assert!(validate_command_name("").is_err());
        assert!(validate_command_name("UPPERCASE").is_err());
        assert!(validate_command_name("has space").is_err());
        assert!(validate_command_name("valid-name").is_ok());
    }

    #[test]
    fn usage_must_be_argument_syntax_without_the_command_name() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["commands"][0]["usage"] = serde_json::json!("repeat [text]");
        let path = write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::InvalidMetadata)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn metadata_limits_count_unicode_characters_and_reject_unsafe_text() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["name"] = serde_json::json!("🦀".repeat(MAX_NAME_CHARS));
        json["commands"][0]["usage"] = serde_json::json!("🦀".repeat(MAX_USAGE_CHARS));
        json["commands"][0]["examples"] = serde_json::json!(["пример"]);
        let path = write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        assert!(validate_manifest_at(&path, Some("echo")).is_ok());

        json["commands"][0]["examples"] = serde_json::json!(["bad\nexample"]);
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::InvalidMetadata)
        ));

        json["commands"][0]["examples"] = serde_json::json!(["good"]);
        json["commands"][0]["description_ru"] = serde_json::json!("bad\u{202e}description");
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::InvalidMetadata)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn id_mismatch_rejected() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        write_manifest(&dir, &valid_manifest_json());
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("wrong-id")),
            Err(ExternalError::IdMismatch)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn empty_commands_rejected() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["commands"] = serde_json::json!([]);
        write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::InvalidCommandCount)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn duplicate_command_rejected() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let json = br#"{
            "schema_version": 2,
            "id": "echo",
            "name": "Echo",
            "version": "0.1.0",
            "author": "Author",
            "entrypoint": "bin/echo-module",
            "capabilities": [],
            "commands": [
                { "name": "repeat", "summary_ru": "A", "description_ru": "A desc", "usage": "<t>", "examples": [] },
                { "name": "repeat", "summary_ru": "B", "description_ru": "B desc", "usage": "<t>", "examples": [] }
            ]
        }"#;
        write_manifest(&dir, json);
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::DuplicateCommand)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn absolute_entrypoint_rejected() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["entrypoint"] = serde_json::json!("/bin/sh");
        write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::InvalidEntrypoint)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn path_traversal_rejected() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["entrypoint"] = serde_json::json!("../../etc/passwd");
        write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::PathEscape)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn duplicate_capabilities_rejected() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["capabilities"] = serde_json::json!(["network", "network"]);
        write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::InvalidCapability)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn unknown_capability_rejected() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["capabilities"] = serde_json::json!(["telegram_rpc"]);
        write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::InvalidCapability)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn discover_valid_modules() {
        let base = temp_dir();
        create_module_dir(&base, "echo");
        write_manifest(&base.join("echo"), &valid_manifest_json());
        let descs = discover_modules(&base).unwrap();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].id, "echo");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn discover_ignores_invalid_dirs() {
        let base = temp_dir();
        create_module_dir(&base, "echo");
        write_manifest(&base.join("echo"), &valid_manifest_json());
        let bad = base.join("BAD_NAME");
        fs::create_dir_all(&bad).unwrap();
        let descs = discover_modules(&base).unwrap();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].id, "echo");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn discover_empty_root() {
        let base = temp_dir();
        let descs = discover_modules(&base).unwrap();
        assert!(descs.is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn discover_nonexistent_root() {
        let base = PathBuf::from("/nonexistent/lavis-modules-test");
        let descs = discover_modules(&base).unwrap();
        assert!(descs.is_empty());
    }

    #[test]
    fn discover_symlinked_entrypoint_rejected() {
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        write_manifest(&dir, &valid_manifest_json());
        let target = dir.join("bin").join("echo-module");
        let symlink_target = dir.join("bin").join("symlinked");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &symlink_target).unwrap();
        }
        let mut json = serde_json::from_slice::<serde_json::Value>(&valid_manifest_json()).unwrap();
        json["entrypoint"] = serde_json::json!("bin/symlinked");
        write_manifest(&dir, &serde_json::to_vec(&json).unwrap());
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::SymlinkEscape)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_dir_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        write_manifest(&dir, &valid_manifest_json());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        let path = dir.join("module.json");
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::UnsafePermissions)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn group_writable_manifest_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let base = temp_dir();
        let dir = create_module_dir(&base, "echo");
        let path = write_manifest(&dir, &valid_manifest_json());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o660)).unwrap();
        assert!(matches!(
            validate_manifest_at(&path, Some("echo")),
            Err(ExternalError::UnsafePermissions)
        ));
        fs::remove_dir_all(&base).unwrap();
    }
}
