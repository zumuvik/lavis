//! Fail-closed inspection and short-lived staging of external module archives.
//!
//! This module does not acquire, execute, or retain an archive after approval is
//! redeemed.  ZIP is deliberately limited to unencrypted stored entries: accepting
//! an encoding we cannot decompress and verify would make the inspection meaningless.

use super::manifest::{
    ExternalModuleDescriptor, validate_display_single_line, validate_manifest_at,
};
use serde::{Serialize, Serializer};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

const EOCD: u32 = 0x0605_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const LOCAL: u32 = 0x0403_4b50;

/// The kind of source represented by an acquired archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Archive,
    PinnedRepository,
}

/// A repository location is useful to show a user, but is never used as a ref.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct PinnedRepository {
    repository: String,
    revision: String,
}
impl std::fmt::Debug for PinnedRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedRepository")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}
impl PinnedRepository {
    pub fn new(repository: String, revision: String) -> Result<Self, SourceInspectionError> {
        if repository.trim().is_empty() || !validate_display_single_line(&repository) {
            return Err(SourceInspectionError::UnsafeRepositoryIdentity);
        }
        if !(revision.len() == 40 || revision.len() == 64)
            || !revision.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(SourceInspectionError::UnpinnedRepository);
        }
        Ok(Self {
            repository,
            revision: revision.to_ascii_lowercase(),
        })
    }
    pub fn repository(&self) -> &str {
        &self.repository
    }
    pub fn revision(&self) -> &str {
        &self.revision
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "repository", rename_all = "snake_case")]
pub enum SourceIdentity {
    Archive,
    PinnedRepository(PinnedRepository),
}
impl SourceIdentity {
    pub fn kind(&self) -> SourceKind {
        match self {
            Self::Archive => SourceKind::Archive,
            Self::PinnedRepository(_) => SourceKind::PinnedRepository,
        }
    }
}
impl std::fmt::Debug for SourceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceIdentity")
            .field("kind", &self.kind())
            .finish_non_exhaustive()
    }
}

/// Transport output. Debug intentionally never exposes untrusted source bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct AcquiredLmod {
    identity: SourceIdentity,
    bytes: Vec<u8>,
}
impl std::fmt::Debug for AcquiredLmod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcquiredLmod")
            .field("identity", &self.identity)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}
impl AcquiredLmod {
    pub fn new(identity: SourceIdentity, bytes: Vec<u8>) -> Self {
        Self { identity, bytes }
    }
    pub fn archive(bytes: Vec<u8>) -> Self {
        Self::new(SourceIdentity::Archive, bytes)
    }
    pub fn len(&self) -> usize {
        self.bytes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
    pub fn source_identity(&self) -> &SourceIdentity {
        &self.identity
    }
}

/// SHA-256 of exact acquired bytes; the typed form prevents mixing it with tokens.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArchiveDigest([u8; 32]);
impl ArchiveDigest {
    pub fn as_hex(&self) -> String {
        hex(&self.0)
    }
    pub fn from_hex(value: &str) -> Result<Self, SourceInspectionError> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(SourceInspectionError::InvalidDigest);
        }
        let mut bytes = [0; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .map_err(|_| SourceInspectionError::InvalidDigest)?;
        }
        Ok(Self(bytes))
    }
}
impl std::fmt::Debug for ArchiveDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ArchiveDigest")
            .field(&self.as_hex())
            .finish()
    }
}
impl Serialize for ArchiveDigest {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&self.as_hex())
    }
}

#[derive(Clone, Debug)]
pub struct InspectionLimits {
    pub max_archive_bytes: usize,
    pub max_files: usize,
    pub max_file_bytes: usize,
    pub max_expanded_bytes: usize,
    pub max_path_depth: usize,
    pub max_path_bytes: usize,
    pub max_compression_ratio: u64,
}
impl Default for InspectionLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 16 * 1024 * 1024,
            max_files: 256,
            max_file_bytes: 4 * 1024 * 1024,
            max_expanded_bytes: 32 * 1024 * 1024,
            max_path_depth: 16,
            max_path_bytes: 1024,
            max_compression_ratio: 100,
        }
    }
}

pub struct InspectionConfig {
    pub staging_root: PathBuf,
    pub limits: InspectionLimits,
}
impl std::fmt::Debug for InspectionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InspectionConfig")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SourceInspectionError {
    #[error("archive is malformed")]
    MalformedArchive,
    #[error("archive is encrypted")]
    EncryptedArchive,
    #[error("archive compression is unsupported")]
    UnsupportedCompression,
    #[error("archive exceeds a size or count limit")]
    LimitExceeded,
    #[error("archive entry path is unsafe")]
    UnsafePath,
    #[error("archive entry types conflict or are unsupported")]
    UnsafeEntryType,
    #[error("archive must contain exactly one root module.json")]
    RootManifest,
    #[error("module manifest is invalid")]
    InvalidManifest,
    #[error("private staging failed")]
    Staging,
    #[error("repository source must use an immutable revision")]
    UnpinnedRepository,
    #[error("repository identity is not safe to display")]
    UnsafeRepositoryIdentity,
    #[error("pending inspection quota exceeded")]
    PendingQuotaExceeded,
    #[error("pending inspection accounting invariant failed")]
    PendingAccountingInvariant,
    #[error("approval token is invalid, expired, or not bound to this request")]
    InvalidToken,
    #[error("secure randomness failed")]
    Entropy,
    #[error("digest is not canonical lowercase SHA-256 hex")]
    InvalidDigest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArchiveStatistics {
    pub archive_bytes: u64,
    pub file_count: u32,
    pub compressed_bytes: u64,
    pub expanded_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionWarning {
    StoredOnlyArchive,
    /// `telegram.raw` grants raw Telegram RPC authority and does not sandbox a module.
    TelegramRawNotSandboxed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InspectionTimes {
    pub inspected_unix_seconds: u64,
    pub expires_unix_seconds: u64,
}

/// Caller-safe review data. It has no staging location or confirmation secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModuleInstallPlan {
    pub source_kind: SourceKind,
    pub source_identity: SourceIdentity,
    pub module_id: String,
    pub module_version: String,
    pub protocol_version: u32,
    pub entrypoint: String,
    pub default_command: Option<String>,
    pub archive_digest: ArchiveDigest,
    pub archive: ArchiveStatistics,
    pub warnings: Vec<InspectionWarning>,
    pub times: InspectionTimes,
    pub capabilities: Vec<String>,
    pub subscriptions: Vec<String>,
    pub telegram_methods: Vec<String>,
    pub actions: Vec<String>,
    pub fingerprint: String,
}
impl ModuleInstallPlan {
    fn from_descriptor(
        d: &ExternalModuleDescriptor,
        identity: SourceIdentity,
        digest: ArchiveDigest,
        archive: ArchiveStatistics,
        now: SystemTime,
        expires: SystemTime,
    ) -> Result<Self, SourceInspectionError> {
        // The manifest validator has already established that this is an
        // executable below the module root. The descriptor intentionally keeps
        // a lexical entrypoint and a canonical module root, so compare their
        // canonical forms before exposing a relative review value.
        let module_dir =
            fs::canonicalize(&d.module_dir).map_err(|_| SourceInspectionError::InvalidManifest)?;
        let canonical_entrypoint =
            fs::canonicalize(&d.entrypoint).map_err(|_| SourceInspectionError::InvalidManifest)?;
        let entrypoint = canonical_entrypoint
            .strip_prefix(&module_dir)
            .map_err(|_| SourceInspectionError::InvalidManifest)?;
        if entrypoint.as_os_str().is_empty()
            || entrypoint.is_absolute()
            || entrypoint
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(SourceInspectionError::InvalidManifest);
        }
        let capabilities = d
            .capabilities
            .iter()
            .map(|x| x.as_str().to_owned())
            .collect();
        let subscriptions = d
            .subscriptions
            .iter()
            .map(|x| match x {
                super::manifest::ExternalSubscription::MessageCreated => "message.created".into(),
                super::manifest::ExternalSubscription::MessageEdited => "message.edited".into(),
            })
            .collect();
        let actions = d
            .actions
            .iter()
            .map(|x| match x {
                super::manifest::ExternalAction::MessageReact => "message.react".into(),
            })
            .collect();
        let telegram_methods = d
            .telegram_methods
            .iter()
            .map(|method| method.spec().name.to_owned())
            .collect();
        let warnings = if d
            .capabilities
            .contains(&super::manifest::ExternalCapability::TelegramRaw)
        {
            vec![
                InspectionWarning::StoredOnlyArchive,
                InspectionWarning::TelegramRawNotSandboxed,
            ]
        } else {
            vec![InspectionWarning::StoredOnlyArchive]
        };
        let mut result = Self {
            source_kind: identity.kind(),
            source_identity: identity,
            module_id: d.id.clone(),
            module_version: d.version.clone(),
            protocol_version: d.protocol_version,
            entrypoint: entrypoint.to_string_lossy().into_owned(),
            default_command: d.default_command.clone(),
            archive_digest: digest,
            archive,
            warnings,
            times: InspectionTimes {
                inspected_unix_seconds: unix_seconds(now),
                expires_unix_seconds: unix_seconds(expires),
            },
            capabilities,
            subscriptions,
            telegram_methods,
            actions,
            fingerprint: String::new(),
        };
        result.fingerprint = result.canonical_fingerprint();
        Ok(result)
    }
    fn canonical_fingerprint(&self) -> String {
        let mut encoded = Vec::new();
        canonical_string(&mut encoded, "lavis-plan-v1");
        canonical_string(
            &mut encoded,
            match self.source_kind {
                SourceKind::Archive => "archive",
                SourceKind::PinnedRepository => "pinned_repository",
            },
        );
        match &self.source_identity {
            SourceIdentity::Archive => canonical_string(&mut encoded, ""),
            SourceIdentity::PinnedRepository(repository) => {
                canonical_string(&mut encoded, &repository.repository);
                canonical_string(&mut encoded, &repository.revision);
            }
        }
        canonical_string(&mut encoded, &self.module_id);
        canonical_string(&mut encoded, &self.module_version);
        encoded.extend_from_slice(&self.protocol_version.to_be_bytes());
        canonical_string(&mut encoded, &self.entrypoint);
        canonical_option(&mut encoded, self.default_command.as_deref());
        encoded.extend_from_slice(&self.archive_digest.0);
        encoded.extend_from_slice(&self.archive.archive_bytes.to_be_bytes());
        encoded.extend_from_slice(&self.archive.file_count.to_be_bytes());
        encoded.extend_from_slice(&self.archive.compressed_bytes.to_be_bytes());
        encoded.extend_from_slice(&self.archive.expanded_bytes.to_be_bytes());
        canonical_list(&mut encoded, &self.capabilities);
        canonical_list(&mut encoded, &self.subscriptions);
        if self.protocol_version == 6 {
            canonical_list(&mut encoded, &self.telegram_methods);
        }
        canonical_list(&mut encoded, &self.actions);
        encoded.extend_from_slice(&(self.warnings.len() as u32).to_be_bytes());
        for warning in &self.warnings {
            canonical_string(
                &mut encoded,
                match warning {
                    InspectionWarning::StoredOnlyArchive => "stored_only_archive",
                    InspectionWarning::TelegramRawNotSandboxed => "telegram_raw_not_sandboxed",
                },
            );
        }
        sha256(&encoded).as_hex()
    }
}
fn canonical_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u64).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}
fn canonical_option(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            out.push(1);
            canonical_string(out, value);
        }
        None => out.push(0),
    }
}
fn canonical_list(out: &mut Vec<u8>, values: &[String]) {
    out.extend_from_slice(&(values.len() as u32).to_be_bytes());
    for value in values {
        canonical_string(out, value);
    }
}
fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub trait Clock {
    fn now(&self) -> SystemTime;
}
pub trait RandomSource {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), SourceInspectionError>;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
pub struct OsRandom;
impl RandomSource for OsRandom {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), SourceInspectionError> {
        getrandom::fill(bytes).map_err(|_| SourceInspectionError::Entropy)
    }
}

/// A private, validated install wrapper.  `wrapper` becomes `None` exactly when
/// ownership is transferred to the installer or when it has been removed.
pub(crate) struct ValidatedStage {
    pub(crate) wrapper: Option<PathBuf>,
}
impl std::fmt::Debug for ValidatedStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ValidatedStage(REDACTED)")
    }
}
impl ValidatedStage {
    fn path(&self) -> Result<&Path, SourceInspectionError> {
        self.wrapper
            .as_deref()
            .ok_or(SourceInspectionError::Staging)
    }

    pub(crate) fn take_wrapper(mut self) -> Option<PathBuf> {
        self.wrapper.take()
    }

    pub(crate) fn cleanup(mut self) -> Result<(), super::installer::StageCleanupError> {
        if let Some(path) = self.wrapper.take() {
            super::installer::cleanup_redeemed_stage(&path)?;
        }
        Ok(())
    }
}
impl Drop for ValidatedStage {
    fn drop(&mut self) {
        if let Some(path) = self.wrapper.take() {
            let _ = remove_tree_no_follow(&path);
        }
    }
}
/// Validated archive staging retained only until approval completes.
pub(crate) struct PendingInspection {
    pub(crate) plan: ModuleInstallPlan,
    pub(crate) stage: ValidatedStage,
}

impl PendingInspection {
    pub(crate) fn expanded_bytes(&self) -> u64 {
        self.plan.archive.expanded_bytes
    }

    pub(crate) fn cleanup(self) -> Result<(), super::installer::StageCleanupError> {
        self.stage.cleanup()
    }
}

/// Inspects acquired source and returns private staged content for approval.
pub(crate) struct ModuleInspector<'a, R> {
    config: &'a InspectionConfig,
    random: R,
}

impl<'a, R: RandomSource> ModuleInspector<'a, R> {
    pub(crate) fn new(config: &'a InspectionConfig, random: R) -> Self {
        Self { config, random }
    }

    pub(crate) fn inspect(
        &mut self,
        source: AcquiredLmod,
        now: SystemTime,
        expires: SystemTime,
    ) -> Result<PendingInspection, SourceInspectionError> {
        inspect_pending(self.config, source, expires, now, &mut self.random)
    }
}

fn inspect_pending(
    config: &InspectionConfig,
    source: AcquiredLmod,
    expires: SystemTime,
    now: SystemTime,
    random: &mut impl RandomSource,
) -> Result<PendingInspection, SourceInspectionError> {
    if source.len() > config.limits.max_archive_bytes {
        return Err(SourceInspectionError::LimitExceeded);
    }
    let stage = create_stage(&config.staging_root, random)?;
    let result = (|| {
        let payload = stage.path()?.join("payload");
        fs::create_dir(&payload).map_err(|_| SourceInspectionError::Staging)?;
        fs::set_permissions(&payload, fs::Permissions::from_mode(0o700))
            .map_err(|_| SourceInspectionError::Staging)?;
        let stats = inspect_into(&payload, &config.limits, &source.bytes)?;
        let d = validate_manifest_at(&payload.join("module.json"), None)
            .map_err(|_| SourceInspectionError::InvalidManifest)?;
        ModuleInstallPlan::from_descriptor(
            &d,
            source.identity,
            sha256(&source.bytes),
            stats,
            now,
            expires,
        )
    })();
    match result {
        Ok(plan) => Ok(PendingInspection { plan, stage }),
        Err(error) => {
            let _ = stage.cleanup();
            Err(error)
        }
    }
}

fn create_stage(
    root: &Path,
    random: &mut impl RandomSource,
) -> Result<ValidatedStage, SourceInspectionError> {
    fs::create_dir_all(root).map_err(|_| SourceInspectionError::Staging)?;
    let meta = fs::symlink_metadata(root).map_err(|_| SourceInspectionError::Staging)?;
    if !meta.file_type().is_dir() || meta.file_type().is_symlink() {
        return Err(SourceInspectionError::Staging);
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))
        .map_err(|_| SourceInspectionError::Staging)?;
    for _ in 0..32 {
        let mut nonce = [0; 16];
        random.fill(&mut nonce)?;
        let path = root.join(format!(".lmod-install-{}", hex(&nonce)));
        match fs::create_dir(&path) {
            Ok(()) => {
                // Own the directory before any fallible post-create operation.
                // Every setup failure below removes this exact path without
                // following links, so an unmarked wrapper cannot leak.
                let stage = ValidatedStage {
                    wrapper: Some(path),
                };
                if fs::set_permissions(stage.path()?, fs::Permissions::from_mode(0o700)).is_err() {
                    if let Ok(path) = stage.path() {
                        let _ = remove_tree_no_follow(path);
                    }
                    return Err(SourceInspectionError::Staging);
                }
                if super::installer::write_stage_marker(stage.path()?, SystemTime::now()).is_err() {
                    if let Ok(path) = stage.path() {
                        let _ = remove_tree_no_follow(path);
                    }
                    return Err(SourceInspectionError::Staging);
                }
                return Ok(stage);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(SourceInspectionError::Staging),
        }
    }
    Err(SourceInspectionError::Staging)
}

#[derive(Clone)]
struct ZipEntry {
    name: String,
    flags: u16,
    method: u16,
    compressed: u64,
    expanded: u64,
    external: u32,
    offset: usize,
}
fn inspect_into(
    stage: &Path,
    limits: &InspectionLimits,
    bytes: &[u8],
) -> Result<ArchiveStatistics, SourceInspectionError> {
    let entries = zip_entries(bytes)?;
    if entries.len() > limits.max_files {
        return Err(SourceInspectionError::LimitExceeded);
    }
    let mut names = BTreeMap::new();
    let mut files = 0u32;
    let mut compressed = 0u64;
    let mut expanded = 0u64;
    let mut manifests = 0;
    for entry in &entries {
        let directory = validate_entry(entry, limits, &mut names)?;
        if !directory {
            files = files
                .checked_add(1)
                .ok_or(SourceInspectionError::LimitExceeded)?;
            compressed = compressed
                .checked_add(entry.compressed)
                .ok_or(SourceInspectionError::LimitExceeded)?;
            expanded = expanded
                .checked_add(entry.expanded)
                .ok_or(SourceInspectionError::LimitExceeded)?;
            if expanded > limits.max_expanded_bytes as u64 {
                return Err(SourceInspectionError::LimitExceeded);
            }
        }
        if entry.name == "module.json" {
            manifests += 1;
        } else if Path::new(entry.name.trim_end_matches('/'))
            .file_name()
            .is_some_and(|name| name == "module.json")
        {
            return Err(SourceInspectionError::RootManifest);
        }
    }
    if manifests != 1 {
        return Err(SourceInspectionError::RootManifest);
    }
    for entry in &entries {
        let directory = entry.name.ends_with('/');
        let destination = stage.join(entry.name.trim_end_matches('/'));
        if directory {
            create_private_dir(&destination)?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            create_private_parents(stage, parent)?;
        }
        let data = entry_data(entry, bytes)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|_| SourceInspectionError::Staging)?;
        file.write_all(data)
            .map_err(|_| SourceInspectionError::Staging)?;
        let executable = ((entry.external >> 16) & 0o111) != 0;
        file.set_permissions(fs::Permissions::from_mode(if executable {
            0o700
        } else {
            0o600
        }))
        .map_err(|_| SourceInspectionError::Staging)?;
    }
    Ok(ArchiveStatistics {
        archive_bytes: bytes.len() as u64,
        file_count: files,
        compressed_bytes: compressed,
        expanded_bytes: expanded,
    })
}
fn create_private_parents(root: &Path, parent: &Path) -> Result<(), SourceInspectionError> {
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| SourceInspectionError::Staging)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        create_private_dir(&current)?;
    }
    Ok(())
}
fn create_private_dir(path: &Path) -> Result<(), SourceInspectionError> {
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| SourceInspectionError::Staging),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let m = fs::symlink_metadata(path).map_err(|_| SourceInspectionError::Staging)?;
            if m.file_type().is_dir() && !m.file_type().is_symlink() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                    .map_err(|_| SourceInspectionError::Staging)
            } else {
                Err(SourceInspectionError::UnsafeEntryType)
            }
        }
        Err(_) => Err(SourceInspectionError::Staging),
    }
}

fn zip_entries(bytes: &[u8]) -> Result<Vec<ZipEntry>, SourceInspectionError> {
    let start = bytes.len().saturating_sub(65_557);
    let eocd = (start..bytes.len().saturating_sub(3))
        .rev()
        .find(|&i| le32(bytes, i).ok() == Some(EOCD))
        .ok_or(SourceInspectionError::MalformedArchive)?;
    if le16(bytes, eocd + 4)? != 0
        || le16(bytes, eocd + 6)? != 0
        || le16(bytes, eocd + 8)? != le16(bytes, eocd + 10)?
    {
        return Err(SourceInspectionError::MalformedArchive);
    }
    let count = le16(bytes, eocd + 10)? as usize;
    let size = le32(bytes, eocd + 12)? as usize;
    let mut at = le32(bytes, eocd + 16)? as usize;
    let end = at
        .checked_add(size)
        .ok_or(SourceInspectionError::MalformedArchive)?;
    if end > eocd {
        return Err(SourceInspectionError::MalformedArchive);
    }
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        if le32(bytes, at)? != CENTRAL {
            return Err(SourceInspectionError::MalformedArchive);
        }
        let flags = le16(bytes, at + 8)?;
        let method = le16(bytes, at + 10)?;
        let compressed = le32(bytes, at + 20)? as u64;
        let expanded = le32(bytes, at + 24)? as u64;
        let nl = le16(bytes, at + 28)? as usize;
        let xl = le16(bytes, at + 30)? as usize;
        let cl = le16(bytes, at + 32)? as usize;
        let next = at
            .checked_add(46 + nl + xl + cl)
            .ok_or(SourceInspectionError::MalformedArchive)?;
        if next > end {
            return Err(SourceInspectionError::MalformedArchive);
        }
        let name = std::str::from_utf8(
            bytes
                .get(at + 46..at + 46 + nl)
                .ok_or(SourceInspectionError::MalformedArchive)?,
        )
        .map_err(|_| SourceInspectionError::UnsafePath)?
        .to_owned();
        result.push(ZipEntry {
            name,
            flags,
            method,
            compressed,
            expanded,
            external: le32(bytes, at + 38)?,
            offset: le32(bytes, at + 42)? as usize,
        });
        at = next;
    }
    if at != end {
        return Err(SourceInspectionError::MalformedArchive);
    }
    Ok(result)
}
fn validate_entry(
    e: &ZipEntry,
    limits: &InspectionLimits,
    names: &mut BTreeMap<String, bool>,
) -> Result<bool, SourceInspectionError> {
    if e.flags & 1 != 0 {
        return Err(SourceInspectionError::EncryptedArchive);
    }
    if e.method != 0 {
        return Err(SourceInspectionError::UnsupportedCompression);
    }
    let directory = e.name.ends_with('/');
    let mode = e.external >> 16;
    let kind = mode & 0o170000;
    if kind != if directory { 0o040000 } else { 0o100000 } || mode & 0o7000 != 0 {
        return Err(SourceInspectionError::UnsafeEntryType);
    }
    let key = e.name.trim_end_matches('/');
    let path = Path::new(key);
    let platform_prefix = key.as_bytes().get(1) == Some(&b':')
        && key
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic());
    // `Path::components` normalizes dot components on Unix, so reject them
    // directly from the archive spelling before constructing a host path.
    let dot_component = key
        .split('/')
        .any(|component| component == "." || component == "..");
    if key.is_empty()
        || key.len() > limits.max_path_bytes
        || e.name.contains('\0')
        || e.name.contains('\\')
        || platform_prefix
        || dot_component
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(SourceInspectionError::UnsafePath);
    }
    if path.components().count() > limits.max_path_depth {
        return Err(SourceInspectionError::LimitExceeded);
    }
    if e.expanded > limits.max_file_bytes as u64
        || (e.compressed == 0 && e.expanded != 0)
        || (e.compressed != 0 && e.expanded.div_ceil(e.compressed) > limits.max_compression_ratio)
    {
        return Err(SourceInspectionError::LimitExceeded);
    }
    if directory && (e.compressed != 0 || e.expanded != 0) {
        return Err(SourceInspectionError::UnsafeEntryType);
    }
    if names.insert(key.to_owned(), directory).is_some()
        || (!directory
            && names
                .keys()
                .any(|name| name.starts_with(&format!("{key}/"))))
    {
        return Err(SourceInspectionError::UnsafeEntryType);
    }
    let mut parent = Path::new(key).parent();
    while let Some(p) = parent {
        if !p.as_os_str().is_empty()
            && names.get(p.to_str().ok_or(SourceInspectionError::UnsafePath)?) == Some(&false)
        {
            return Err(SourceInspectionError::UnsafeEntryType);
        }
        parent = p.parent();
    }
    Ok(directory)
}
fn entry_data<'a>(e: &ZipEntry, bytes: &'a [u8]) -> Result<&'a [u8], SourceInspectionError> {
    if le32(bytes, e.offset)? != LOCAL {
        return Err(SourceInspectionError::MalformedArchive);
    }
    if le16(bytes, e.offset + 6)? != e.flags
        || le16(bytes, e.offset + 8)? != e.method
        || le32(bytes, e.offset + 18)? as u64 != e.compressed
        || le32(bytes, e.offset + 22)? as u64 != e.expanded
    {
        return Err(SourceInspectionError::MalformedArchive);
    }
    let nl = le16(bytes, e.offset + 26)? as usize;
    let xl = le16(bytes, e.offset + 28)? as usize;
    if bytes.get(e.offset + 30..e.offset + 30 + nl) != Some(e.name.as_bytes()) {
        return Err(SourceInspectionError::MalformedArchive);
    }
    let data = e
        .offset
        .checked_add(30 + nl + xl)
        .ok_or(SourceInspectionError::MalformedArchive)?;
    let end = data
        .checked_add(
            usize::try_from(e.compressed).map_err(|_| SourceInspectionError::MalformedArchive)?,
        )
        .ok_or(SourceInspectionError::MalformedArchive)?;
    bytes
        .get(data..end)
        .ok_or(SourceInspectionError::MalformedArchive)
}
fn le16(b: &[u8], at: usize) -> Result<u16, SourceInspectionError> {
    Ok(u16::from_le_bytes(
        b.get(at..at + 2)
            .ok_or(SourceInspectionError::MalformedArchive)?
            .try_into()
            .map_err(|_| SourceInspectionError::MalformedArchive)?,
    ))
}
fn le32(b: &[u8], at: usize) -> Result<u32, SourceInspectionError> {
    Ok(u32::from_le_bytes(
        b.get(at..at + 4)
            .ok_or(SourceInspectionError::MalformedArchive)?
            .try_into()
            .map_err(|_| SourceInspectionError::MalformedArchive)?,
    ))
}
/// Remove exactly this tree. `symlink_metadata` means a link is unlinked, never
/// traversed; concurrent removal is an already-successful cleanup.
fn remove_tree_no_follow(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        for item in fs::read_dir(path)? {
            remove_tree_no_follow(&item?.path())?;
        }
        match fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    } else {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// SHA-256 is local to keep the archive boundary dependency-free.
fn sha256(data: &[u8]) -> ArchiveDigest {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut message = data.to_vec();
    let bits = (message.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());
    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    for block in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, part) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(part.try_into().unwrap_or([0; 4]));
        }
        for i in 16..64 {
            w[i] = w[i - 16]
                .wrapping_add(
                    w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3),
                )
                .wrapping_add(w[i - 7])
                .wrapping_add(
                    w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10),
                );
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut q) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let t1 = q
                .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let t2 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add((a & b) ^ (a & c) ^ (b & c));
            q = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(q);
    }
    let mut result = [0; 32];
    for (i, word) in h.iter().enumerate() {
        result[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    ArchiveDigest(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::MetadataExt};

    #[derive(Clone)]
    struct Entry {
        name: String,
        data: Vec<u8>,
        flags: u16,
        method: u16,
        mode: u32,
        expanded: u32,
    }

    fn put16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn put32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn zip(entries: &[Entry]) -> Vec<u8> {
        let mut output = Vec::new();
        let mut offsets = Vec::new();
        for entry in entries {
            offsets.push(output.len() as u32);
            put32(&mut output, LOCAL);
            put16(&mut output, 20);
            put16(&mut output, entry.flags);
            put16(&mut output, entry.method);
            put32(&mut output, 0);
            put32(&mut output, 0);
            put32(&mut output, entry.data.len() as u32);
            put32(&mut output, entry.expanded);
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
            put16(&mut output, entry.flags);
            put16(&mut output, entry.method);
            put32(&mut output, 0);
            put32(&mut output, 0);
            put32(&mut output, entry.data.len() as u32);
            put32(&mut output, entry.expanded);
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

    fn file(name: &str, data: &[u8]) -> Entry {
        Entry {
            name: name.into(),
            data: data.into(),
            flags: 0,
            method: 0,
            mode: 0o100644,
            expanded: data.len() as u32,
        }
    }

    fn manifest() -> Entry {
        file(
            "module.json",
            br#"{"schema_version":2,"id":"test","name":"Test","version":"1","author":"A","entrypoint":"run","commands":[{"name":"go","summary_ru":"x","description_ru":"x","usage":"<value>"}]}"#,
        )
    }

    fn valid_archive() -> Vec<u8> {
        let mut run = file("run", b"#!/bin/sh");
        run.mode = 0o100755;
        zip(&[manifest(), run])
    }

    fn v5_archive() -> Vec<u8> {
        let manifest = file(
            "module.json",
            br#"{"schema_version":5,"id":"status","name":"Status","version":"1","author":"A","entrypoint":"run","capabilities":["telegram.account.status"],"commands":[{"name":"go","summary_ru":"x","description_ru":"x","usage":"<value>"}]}"#,
        );
        let mut run = file("run", b"#!/bin/sh");
        run.mode = 0o100755;
        zip(&[manifest, run])
    }

    fn v6_archive(method: &str) -> Vec<u8> {
        let capabilities = if method == "raw.invoke" {
            r#"["telegram.raw"]"#
        } else {
            "[]"
        };
        let manifest = file(
            "module.json",
            format!(
                "{{\"schema_version\":6,\"id\":\"raw\",\"name\":\"Raw\",\"version\":\"1\",\"author\":\"A\",\"entrypoint\":\"run\",\"capabilities\":{capabilities},\"telegram_methods\":[\"{method}\"],\"commands\":[{{\"name\":\"go\",\"summary_ru\":\"x\",\"description_ru\":\"x\",\"usage\":\"<value>\"}}]}}"
            )
            .as_bytes(),
        );
        let mut run = file("run", b"#!/bin/sh");
        run.mode = 0o100755;
        zip(&[manifest, run])
    }

    struct TestRandom(u8);

    impl RandomSource for TestRandom {
        fn fill(&mut self, bytes: &mut [u8]) -> Result<(), SourceInspectionError> {
            bytes.fill(self.0);
            self.0 = self.0.wrapping_add(1);
            Ok(())
        }
    }

    fn limits() -> InspectionLimits {
        InspectionLimits {
            max_archive_bytes: 100_000,
            max_files: 4,
            max_file_bytes: 10_000,
            max_expanded_bytes: 20_000,
            max_path_depth: 4,
            max_path_bytes: 32,
            max_compression_ratio: 10,
        }
    }

    fn root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "lavis-source-inspection-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn zip_rejects_encryption_compression_paths_symlinks_and_unsafe_types() {
        let mut encrypted = file("entry", b"x");
        encrypted.flags = 1;
        assert_eq!(
            validate_entry(
                &zip_entries(&zip(&[encrypted])).unwrap()[0],
                &limits(),
                &mut BTreeMap::new()
            ),
            Err(SourceInspectionError::EncryptedArchive)
        );
        let mut compressed = file("entry", b"x");
        compressed.method = 8;
        assert_eq!(
            validate_entry(
                &zip_entries(&zip(&[compressed])).unwrap()[0],
                &limits(),
                &mut BTreeMap::new()
            ),
            Err(SourceInspectionError::UnsupportedCompression)
        );
        for path in ["../escape", "/absolute", "a\\b", "a/./b", "a/../../b"] {
            assert_eq!(
                validate_entry(
                    &zip_entries(&zip(&[file(path, b"")])).unwrap()[0],
                    &limits(),
                    &mut BTreeMap::new()
                ),
                Err(SourceInspectionError::UnsafePath)
            );
        }
        let mut link = file("link", b"");
        link.mode = 0o120777;
        assert_eq!(
            validate_entry(
                &zip_entries(&zip(&[link])).unwrap()[0],
                &limits(),
                &mut BTreeMap::new()
            ),
            Err(SourceInspectionError::UnsafeEntryType)
        );
    }

    #[test]
    fn zip_and_archive_limits_and_root_manifest_are_enforced() {
        let mut too_large = file("entry", b"x");
        too_large.expanded = 10_001;
        assert_eq!(
            validate_entry(
                &zip_entries(&zip(&[too_large])).unwrap()[0],
                &limits(),
                &mut BTreeMap::new()
            ),
            Err(SourceInspectionError::LimitExceeded)
        );
        let stage_root = root("limits");
        let mut random = TestRandom(1);
        let stage = create_stage(&stage_root, &mut random).unwrap();
        let mut file_limit = limits();
        file_limit.max_files = 1;
        assert_eq!(
            inspect_into(
                stage.path().unwrap().join("payload").as_path(),
                &file_limit,
                &valid_archive()
            ),
            Err(SourceInspectionError::LimitExceeded)
        );
        assert_eq!(
            inspect_into(
                stage.path().unwrap().join("payload").as_path(),
                &limits(),
                &zip(&[file("nested/module.json", b"{}")])
            ),
            Err(SourceInspectionError::RootManifest)
        );
        stage.cleanup().unwrap();
        let _ = fs::remove_dir(stage_root);
    }

    #[test]
    fn plans_are_deterministic_and_staging_is_private_and_cleaned() {
        let root = root("plans");
        let config = InspectionConfig {
            staging_root: root.clone(),
            limits: limits(),
        };
        let mut random = TestRandom(1);
        let first = inspect_pending(
            &config,
            AcquiredLmod::archive(valid_archive()),
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH,
            &mut random,
        )
        .unwrap();
        let second = inspect_pending(
            &config,
            AcquiredLmod::archive(valid_archive()),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(9),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10),
            &mut random,
        )
        .unwrap();
        assert_eq!(first.plan.fingerprint, second.plan.fingerprint);
        assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
        let wrapper = first.stage.path().unwrap().to_path_buf();
        assert!(wrapper.join("owner.json").is_file());
        first.cleanup().unwrap();
        second.cleanup().unwrap();
        assert!(!wrapper.exists());
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn v5_plan_exposes_account_status_capability() {
        let root = root("v5-plan");
        let config = InspectionConfig {
            staging_root: root.clone(),
            limits: limits(),
        };
        let mut random = TestRandom(1);
        let pending = inspect_pending(
            &config,
            AcquiredLmod::archive(v5_archive()),
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH,
            &mut random,
        )
        .unwrap();
        assert!(
            pending
                .plan
                .capabilities
                .contains(&"telegram.account.status".to_owned())
        );
        let without_capability = ModuleInstallPlan {
            capabilities: Vec::new(),
            ..pending.plan.clone()
        };
        assert_ne!(
            pending.plan.fingerprint,
            without_capability.canonical_fingerprint()
        );
        pending.cleanup().unwrap();
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn v6_plan_fingerprints_explicit_raw_method_grants() {
        let root = root("v6-plan");
        let config = InspectionConfig {
            staging_root: root.clone(),
            limits: limits(),
        };
        let mut random = TestRandom(1);
        let first = inspect_pending(
            &config,
            AcquiredLmod::archive(v6_archive("raw.invoke")),
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH,
            &mut random,
        )
        .unwrap();
        let second = inspect_pending(
            &config,
            AcquiredLmod::archive(v6_archive("contacts.getContacts")),
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH,
            &mut random,
        )
        .unwrap();
        assert_eq!(first.plan.telegram_methods, vec!["raw.invoke".to_owned()]);
        assert!(
            first
                .plan
                .warnings
                .contains(&InspectionWarning::TelegramRawNotSandboxed)
        );
        let changed_grant = ModuleInstallPlan {
            telegram_methods: vec!["contacts.getContacts".to_owned()],
            ..first.plan.clone()
        };
        assert_ne!(
            first.plan.fingerprint,
            changed_grant.canonical_fingerprint()
        );
        assert_ne!(first.plan.fingerprint, second.plan.fingerprint);
        first.cleanup().unwrap();
        second.cleanup().unwrap();
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn legacy_fingerprint_has_its_pre_v6_golden_value() {
        let plan = ModuleInstallPlan {
            source_kind: SourceKind::Archive,
            source_identity: SourceIdentity::Archive,
            module_id: "legacy".to_owned(),
            module_version: "1".to_owned(),
            protocol_version: 5,
            entrypoint: "run".to_owned(),
            default_command: None,
            archive_digest: ArchiveDigest([0; 32]),
            archive: ArchiveStatistics {
                archive_bytes: 1,
                file_count: 2,
                compressed_bytes: 3,
                expanded_bytes: 4,
            },
            warnings: vec![InspectionWarning::StoredOnlyArchive],
            times: InspectionTimes {
                inspected_unix_seconds: 0,
                expires_unix_seconds: 0,
            },
            capabilities: vec![],
            subscriptions: vec![],
            telegram_methods: vec!["account.updateStatus".to_owned()],
            actions: vec![],
            fingerprint: String::new(),
        };
        assert_eq!(
            plan.canonical_fingerprint(),
            "d920f19f1446cd1cc08b61e173105b5a4ecb79ce528581b4c7f3910c9358f19e"
        );
    }

    #[test]
    fn failed_inspection_leaves_no_wrapper_and_cleanup_never_follows_symlinks() {
        let staging_root = root("cleanup");
        let config = InspectionConfig {
            staging_root: staging_root.clone(),
            limits: limits(),
        };
        let mut random = TestRandom(1);
        assert!(
            inspect_pending(
                &config,
                AcquiredLmod::archive(vec![1, 2, 3]),
                SystemTime::UNIX_EPOCH,
                SystemTime::UNIX_EPOCH,
                &mut random
            )
            .is_err()
        );
        assert!(fs::read_dir(&staging_root).unwrap().next().is_none());
        let stage = create_stage(&staging_root, &mut random).unwrap();
        let wrapper = stage.path().unwrap().to_path_buf();
        let outside = root("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, wrapper.join("link")).unwrap();
        assert!(stage.cleanup().is_err());
        assert!(outside.join("keep").exists());
        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(&staging_root);
    }

    #[test]
    fn marker_creation_failure_path_keeps_cleanup_owned_by_the_stage() {
        let root = root("marker-failure");
        let mut random = TestRandom(1);
        let stage = create_stage(&root, &mut random).unwrap();
        assert!(
            crate::external_modules::installer::write_stage_marker(
                stage.path().unwrap(),
                SystemTime::UNIX_EPOCH
            )
            .is_err()
        );
        let wrapper = stage.path().unwrap().to_path_buf();
        stage.cleanup().unwrap();
        assert!(!wrapper.exists());
        let _ = fs::remove_dir(root);
    }
}
