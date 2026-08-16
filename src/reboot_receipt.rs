//! Durable, Telegram-independent acknowledgement of a reboot command.
//!
//! The receipt intentionally contains only the data needed by a future
//! transport adapter to edit the command message after the process restarts.

use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    future::Future,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

use crate::i18n::{Locale, RebootText, reboot_text};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

pub const RECEIPT_VERSION: u32 = 1;
pub const RECEIPT_TTL_MILLIS: u64 = 10 * 60 * 1_000;
pub const MAX_FUTURE_MILLIS: u64 = 60 * 1_000;
const MAX_RECEIPT_BYTES: usize = 1024;

/// A stable, transport-neutral target for the message to edit after restart.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReceiptTarget {
    SelfUser,
    User { id: i64, access_hash: i64 },
    Chat { id: i64 },
    Channel { id: i64, access_hash: i64 },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRebootReceipt {
    version: u32,
    target: ReceiptTarget,
    message_id: i32,
    started_unix_ms: u64,
}

impl PendingRebootReceipt {
    pub fn new(
        target: ReceiptTarget,
        message_id: i32,
        started_unix_ms: u64,
    ) -> Result<Self, ReceiptValidationError> {
        let receipt = Self {
            version: RECEIPT_VERSION,
            target,
            message_id,
            started_unix_ms,
        };
        receipt.validate_shape()?;
        Ok(receipt)
    }

    pub fn target(&self) -> &ReceiptTarget {
        &self.target
    }

    pub fn message_id(&self) -> i32 {
        self.message_id
    }

    pub fn started_unix_ms(&self) -> u64 {
        self.started_unix_ms
    }

    pub fn elapsed_millis(&self, now_unix_ms: u64) -> Result<u64, ReceiptValidationError> {
        if self.started_unix_ms > now_unix_ms.saturating_add(MAX_FUTURE_MILLIS) {
            return Err(ReceiptValidationError::TooFarInFuture);
        }
        Ok(now_unix_ms.saturating_sub(self.started_unix_ms))
    }

    fn validate_shape(&self) -> Result<(), ReceiptValidationError> {
        if self.version != RECEIPT_VERSION {
            return Err(ReceiptValidationError::UnsupportedVersion(self.version));
        }
        if self.message_id <= 0 {
            return Err(ReceiptValidationError::InvalidMessageId);
        }
        match self.target {
            ReceiptTarget::SelfUser => Ok(()),
            ReceiptTarget::User { id, .. } | ReceiptTarget::Channel { id, .. } => {
                if id <= 0 {
                    Err(ReceiptValidationError::InvalidTargetId)
                } else {
                    Ok(())
                }
            }
            ReceiptTarget::Chat { id } if id <= 0 => Err(ReceiptValidationError::InvalidTargetId),
            ReceiptTarget::Chat { .. } => Ok(()),
        }
    }
}

pub trait Clock {
    fn unix_millis(&self) -> Result<u64, ClockError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_millis(&self) -> Result<u64, ClockError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .map_err(|_| ClockError::BeforeUnixEpoch)
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ClockError {
    #[error("system clock is before the Unix epoch")]
    BeforeUnixEpoch,
}

/// Result of one transport-specific attempt to edit a reboot receipt message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptEditOutcome {
    Applied,
    AlreadyApplied,
    Temporary,
    Terminal,
}

/// A single edit intent. Transport adapters should install any local expected-edit
/// suppression using this data immediately before issuing their concrete edit.
#[derive(Clone, PartialEq, Eq)]
pub struct RebootReceiptEditIntent {
    pub receipt: PendingRebootReceipt,
    pub text: String,
}

impl std::fmt::Debug for ReceiptTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SelfUser => formatter.write_str("SelfUser"),
            Self::User { id, .. } => formatter.debug_struct("User").field("id", id).finish(),
            Self::Chat { id } => formatter.debug_struct("Chat").field("id", id).finish(),
            Self::Channel { id, .. } => formatter.debug_struct("Channel").field("id", id).finish(),
        }
    }
}

impl std::fmt::Debug for PendingRebootReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingRebootReceipt")
            .field("version", &self.version)
            .field("target", &self.target)
            .field("message_id", &self.message_id)
            .field("started_unix_ms", &self.started_unix_ms)
            .finish()
    }
}

impl std::fmt::Debug for RebootReceiptEditIntent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RebootReceiptEditIntent")
            .field("receipt", &self.receipt)
            .field("text", &"[redacted]")
            .finish()
    }
}

/// Transport seam for completing a reboot receipt.
pub trait RebootReceiptEditor {
    fn edit_reboot_receipt(
        &mut self,
        intent: RebootReceiptEditIntent,
    ) -> impl Future<Output = ReceiptEditOutcome> + Send;
}

/// Backoff seam, kept separate from the editor so coordinator tests need no time.
pub trait RebootReceiptSleeper {
    fn sleep(&mut self, duration: Duration) -> impl Future<Output = ()> + Send;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TokioSleeper;

impl RebootReceiptSleeper for TokioSleeper {
    fn sleep(&mut self, duration: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(duration)
    }
}

pub const REBOOT_RECEIPT_MAX_ATTEMPTS: u8 = 3;
pub const REBOOT_RECEIPT_RETRY_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebootReceiptRetryPolicy {
    pub max_attempts: u8,
    pub backoff: Duration,
}

impl Default for RebootReceiptRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: REBOOT_RECEIPT_MAX_ATTEMPTS,
            backoff: REBOOT_RECEIPT_RETRY_BACKOFF,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebootReceiptCompletion {
    Absent,
    Discarded,
    Applied,
    AlreadyApplied,
    Terminal,
    TemporaryExhausted,
}

#[derive(Debug, Error)]
pub enum RebootReceiptCoordinatorError {
    #[error(transparent)]
    Store(#[from] ReceiptStoreError),
    #[error(transparent)]
    Clock(#[from] ClockError),
    #[error(transparent)]
    Validation(#[from] ReceiptValidationError),
}

/// Complete one valid persisted receipt with bounded retries.
///
/// Temporary failures deliberately leave the receipt armed, so a later process
/// start can retry it. All other edit outcomes consume it.
pub async fn complete_pending_reboot_receipt<C, E, S>(
    store: &RebootReceiptStore,
    clock: &C,
    editor: &mut E,
    sleeper: &mut S,
    policy: RebootReceiptRetryPolicy,
    locale: Locale,
) -> Result<RebootReceiptCompletion, RebootReceiptCoordinatorError>
where
    C: Clock,
    E: RebootReceiptEditor,
    S: RebootReceiptSleeper,
{
    let pending = match store.load(clock).await? {
        LoadOutcome::Absent => return Ok(RebootReceiptCompletion::Absent),
        LoadOutcome::Discarded(_) => return Ok(RebootReceiptCompletion::Discarded),
        LoadOutcome::Pending(pending) => pending,
    };
    let attempts = policy.max_attempts.max(1);
    for attempt in 0..attempts {
        let elapsed = pending.receipt.elapsed_millis(clock.unix_millis()?)?;
        let intent = RebootReceiptEditIntent {
            receipt: pending.receipt.clone(),
            text: reboot_completion_text(locale, elapsed),
        };
        match editor.edit_reboot_receipt(intent).await {
            ReceiptEditOutcome::Applied => {
                store.remove().await?;
                return Ok(RebootReceiptCompletion::Applied);
            }
            ReceiptEditOutcome::AlreadyApplied => {
                store.remove().await?;
                return Ok(RebootReceiptCompletion::AlreadyApplied);
            }
            ReceiptEditOutcome::Terminal => {
                store.remove().await?;
                return Ok(RebootReceiptCompletion::Terminal);
            }
            ReceiptEditOutcome::Temporary if attempt + 1 < attempts => {
                sleeper.sleep(policy.backoff).await;
            }
            ReceiptEditOutcome::Temporary => {
                return Ok(RebootReceiptCompletion::TemporaryExhausted);
            }
        }
    }
    unreachable!("retry attempt count is always at least one")
}

pub fn reboot_completion_text(locale: Locale, elapsed_millis: u64) -> String {
    reboot_text(locale, RebootText::Completed, Some(elapsed_millis / 1_000))
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ReceiptValidationError {
    #[error("unsupported reboot receipt version {0}")]
    UnsupportedVersion(u32),
    #[error("reboot receipt message id must be positive")]
    InvalidMessageId,
    #[error("reboot receipt target id must be positive")]
    InvalidTargetId,
    #[error("reboot receipt timestamp is too far in the future")]
    TooFarInFuture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedReceipt {
    pub receipt: PendingRebootReceipt,
    pub elapsed_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardReason {
    Malformed,
    UnsupportedVersion(u32),
    Invalid(ReceiptValidationError),
    Expired,
    UnsafeState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    Absent,
    Pending(LoadedReceipt),
    Discarded(DiscardReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmOutcome {
    Armed,
    Conflict,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TemporaryCleanup {
    pub removed: u8,
    pub failures: u8,
}

#[derive(Debug, Error)]
pub enum ReceiptStoreError {
    #[error("unsafe reboot receipt state: {detail}")]
    UnsafeState { detail: &'static str },
    #[error("reboot receipt storage is unavailable while {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("reboot receipt may have been installed, but syncing its directory failed: {source}")]
    ArmDurabilityUnknown {
        #[source]
        source: io::Error,
    },
    #[error("reboot receipt may have been deleted, but syncing its directory failed: {source}")]
    DeleteDurabilityUnknown {
        #[source]
        source: io::Error,
    },
    #[error("reboot receipt task was cancelled")]
    TaskCancelled,
    #[error(transparent)]
    Validation(#[from] ReceiptValidationError),
}

/// One-file store. All async methods isolate filesystem work in `spawn_blocking`.
#[derive(Debug, Clone)]
pub struct RebootReceiptStore {
    path: PathBuf,
}

impl RebootReceiptStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn load<C: Clock>(&self, clock: &C) -> Result<LoadOutcome, ReceiptStoreError> {
        let now = clock.unix_millis().map_err(|error| ReceiptStoreError::Io {
            operation: "reading system clock",
            source: io::Error::other(error),
        })?;
        self.load_at(now).await
    }

    pub async fn load_at(&self, now_unix_ms: u64) -> Result<LoadOutcome, ReceiptStoreError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || load_at_blocking(&path, now_unix_ms))
            .await
            .map_err(|_| ReceiptStoreError::TaskCancelled)?
    }

    pub async fn cleanup_stale_temporaries(&self) -> Result<TemporaryCleanup, ReceiptStoreError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || cleanup_stale_temporaries_blocking(&path))
            .await
            .map_err(|_| ReceiptStoreError::TaskCancelled)?
    }

    pub async fn arm(
        &self,
        receipt: PendingRebootReceipt,
    ) -> Result<ArmOutcome, ReceiptStoreError> {
        receipt.validate_shape()?;
        let bytes = serialize(&receipt)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || arm_blocking(&path, &bytes))
            .await
            .map_err(|_| ReceiptStoreError::TaskCancelled)?
    }

    pub async fn remove(&self) -> Result<bool, ReceiptStoreError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || remove_blocking(&path))
            .await
            .map_err(|_| ReceiptStoreError::TaskCancelled)?
    }
}

fn serialize(receipt: &PendingRebootReceipt) -> Result<Vec<u8>, ReceiptStoreError> {
    let bytes = serde_json::to_vec(receipt).map_err(|error| ReceiptStoreError::Io {
        operation: "serializing receipt",
        source: io::Error::other(error),
    })?;
    if bytes.len() > MAX_RECEIPT_BYTES {
        return Err(ReceiptStoreError::Io {
            operation: "serializing receipt",
            source: io::Error::other("receipt exceeds size limit"),
        });
    }
    Ok(bytes)
}

fn load_at_blocking(path: &Path, now: u64) -> Result<LoadOutcome, ReceiptStoreError> {
    ensure_parent(path)?;
    let cleanup = cleanup_stale_temporaries_blocking(path)?;
    if cleanup.failures != 0 {
        tracing::warn!(
            event = "reboot_receipt_stale_temp_cleanup_failed",
            failures = cleanup.failures,
            "Could not remove stale reboot receipt temporary files"
        );
    }
    let bytes = match read_file(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(LoadOutcome::Absent),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            return discard(path, DiscardReason::UnsafeState);
        }
        Err(error) => return Err(io_error("reading receipt", error)),
    };
    let receipt: PendingRebootReceipt = match serde_json::from_slice(&bytes) {
        Ok(receipt) => receipt,
        Err(_) => return discard(path, DiscardReason::Malformed),
    };
    if let Err(error) = receipt.validate_shape() {
        let reason = match error {
            ReceiptValidationError::UnsupportedVersion(version) => {
                DiscardReason::UnsupportedVersion(version)
            }
            other => DiscardReason::Invalid(other),
        };
        return discard(path, reason);
    }
    let elapsed = match receipt.elapsed_millis(now) {
        Ok(elapsed) => elapsed,
        Err(error) => return discard(path, DiscardReason::Invalid(error)),
    };
    if elapsed > RECEIPT_TTL_MILLIS {
        return discard(path, DiscardReason::Expired);
    }
    Ok(LoadOutcome::Pending(LoadedReceipt {
        receipt,
        elapsed_millis: elapsed,
    }))
}

fn arm_blocking(path: &Path, bytes: &[u8]) -> Result<ArmOutcome, ReceiptStoreError> {
    let parent = ensure_parent(path)?;
    let cleanup = cleanup_stale_temporaries_blocking(path)?;
    if cleanup.failures != 0 {
        tracing::warn!(
            event = "reboot_receipt_stale_temp_cleanup_failed",
            failures = cleanup.failures,
            "Could not remove stale reboot receipt temporary files"
        );
    }
    let temporary = temporary_path(parent)?;
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .map_err(|error| io_error("creating temporary receipt", error))?;
        #[cfg(unix)]
        if file
            .metadata()
            .map_err(|error| io_error("verifying temporary receipt", error))?
            .permissions()
            .mode()
            & 0o777
            != 0o600
        {
            return Err(io_error(
                "verifying temporary receipt",
                io::Error::other("permissions are not 0600"),
            ));
        }
        file.write_all(bytes)
            .map_err(|error| io_error("writing temporary receipt", error))?;
        file.sync_all()
            .map_err(|error| io_error("syncing temporary receipt", error))?;
        drop(file);

        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                // The link is the commit point. Cleanup can no longer report an
                // ordinary unarmed failure.
                let cleanup_failed = fs::remove_file(&temporary).err();
                sync_directory(parent)
                    .map_err(|source| ReceiptStoreError::ArmDurabilityUnknown { source })?;
                if let Some(source) = cleanup_failed {
                    return Err(ReceiptStoreError::ArmDurabilityUnknown { source });
                }
                Ok(ArmOutcome::Armed)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(ArmOutcome::Conflict),
            Err(error) => Err(io_error("atomically installing receipt", error)),
        }
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn discard(path: &Path, reason: DiscardReason) -> Result<LoadOutcome, ReceiptStoreError> {
    remove_blocking(path)?;
    Ok(LoadOutcome::Discarded(reason))
}

fn remove_blocking(path: &Path) -> Result<bool, ReceiptStoreError> {
    let parent = ensure_parent(path)?;
    match fs::remove_file(path) {
        Ok(()) => sync_directory(parent)
            .map_err(|source| ReceiptStoreError::DeleteDurabilityUnknown { source })
            .map(|()| true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("removing receipt", error)),
    }
}

fn ensure_parent(path: &Path) -> Result<&Path, ReceiptStoreError> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| ReceiptStoreError::Io {
            operation: "resolving receipt state directory",
            source: io::Error::new(io::ErrorKind::InvalidInput, "receipt path has no parent"),
        })?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                return Err(ReceiptStoreError::UnsafeState {
                    detail: "state directory is not a real directory",
                });
            }
            #[cfg(unix)]
            if metadata.permissions().mode() & 0o777 != 0o700 {
                return Err(ReceiptStoreError::UnsafeState {
                    detail: "state directory permissions are not 0700",
                });
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(parent)
                .map_err(|error| io_error("creating receipt state directory", error))?;
            #[cfg(unix)]
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .map_err(|error| io_error("securing receipt state directory", error))?;
            let metadata = fs::symlink_metadata(parent)
                .map_err(|error| io_error("verifying receipt state directory", error))?;
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                return Err(ReceiptStoreError::UnsafeState {
                    detail: "state directory is not a real directory",
                });
            }
            #[cfg(unix)]
            if metadata.permissions().mode() & 0o777 != 0o700 {
                return Err(ReceiptStoreError::UnsafeState {
                    detail: "state directory permissions are not 0700",
                });
            }
        }
        Err(error) => return Err(io_error("checking receipt state directory", error)),
    }
    Ok(parent)
}

fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path).map_err(|error| {
        if error.raw_os_error() == Some(libc::ELOOP) {
            io::Error::new(io::ErrorKind::InvalidData, "receipt is a symlink")
        } else {
            error
        }
    })?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "receipt is not a regular file",
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "receipt permissions are not 0600",
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_RECEIPT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECEIPT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "receipt exceeds size limit",
        ));
    }
    Ok(bytes)
}

fn cleanup_stale_temporaries_blocking(path: &Path) -> Result<TemporaryCleanup, ReceiptStoreError> {
    let parent = ensure_parent(path)?;
    let mut outcome = TemporaryCleanup::default();
    for entry in fs::read_dir(parent)
        .map_err(|error| io_error("listing receipt temporary files", error))?
        .take(16)
    {
        let entry = entry.map_err(|error| io_error("reading receipt temporary file", error))?;
        let name = entry.file_name();
        if !is_receipt_temporary(&name) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => outcome.removed = outcome.removed.saturating_add(1),
            Err(_) => outcome.failures = outcome.failures.saturating_add(1),
        }
    }
    Ok(outcome)
}

fn is_receipt_temporary(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(nonce) = name
        .strip_prefix(".reboot-receipt-")
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn temporary_path(parent: &Path) -> Result<PathBuf, ReceiptStoreError> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| ReceiptStoreError::Io {
        operation: "generating receipt temporary name",
        source: io::Error::other(error.to_string()),
    })?;
    Ok(parent.join(format!(
        ".reboot-receipt-{:x}.tmp",
        u128::from_le_bytes(nonce)
    )))
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

fn io_error(operation: &'static str, source: io::Error) -> ReceiptStoreError {
    ReceiptStoreError::Io { operation, source }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const NOW: u64 = 1_700_000_000_000;

    struct FakeClock(std::cell::RefCell<Vec<u64>>);

    impl FakeClock {
        fn new(values: impl IntoIterator<Item = u64>) -> Self {
            Self(std::cell::RefCell::new(values.into_iter().collect()))
        }
    }

    impl Clock for FakeClock {
        fn unix_millis(&self) -> Result<u64, ClockError> {
            Ok(self.0.borrow_mut().remove(0))
        }
    }

    struct FakeEditor {
        outcomes: std::collections::VecDeque<ReceiptEditOutcome>,
        intents: Vec<RebootReceiptEditIntent>,
    }

    impl FakeEditor {
        fn new(outcomes: impl IntoIterator<Item = ReceiptEditOutcome>) -> Self {
            Self {
                outcomes: outcomes.into_iter().collect(),
                intents: Vec::new(),
            }
        }
    }

    impl RebootReceiptEditor for FakeEditor {
        fn edit_reboot_receipt(
            &mut self,
            intent: RebootReceiptEditIntent,
        ) -> impl Future<Output = ReceiptEditOutcome> + Send {
            self.intents.push(intent);
            std::future::ready(
                self.outcomes
                    .pop_front()
                    .unwrap_or(ReceiptEditOutcome::Terminal),
            )
        }
    }

    #[derive(Default)]
    struct FakeSleeper(Vec<Duration>);

    impl RebootReceiptSleeper for FakeSleeper {
        fn sleep(&mut self, duration: Duration) -> impl Future<Output = ()> + Send {
            self.0.push(duration);
            std::future::ready(())
        }
    }

    fn receipt() -> PendingRebootReceipt {
        PendingRebootReceipt::new(
            ReceiptTarget::Channel {
                id: 42,
                access_hash: -7,
            },
            99,
            NOW,
        )
        .unwrap()
    }

    fn path() -> PathBuf {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "lavis-reboot-receipt-{:x}",
            u64::from_le_bytes(nonce)
        ));
        fs::create_dir(&directory).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        directory.join("reboot-receipt.json")
    }

    #[test]
    fn serializes_only_stable_peer_and_message_data() {
        let json = serde_json::to_string(&receipt()).unwrap();
        assert!(json.contains("channel"));
        assert!(json.contains("access_hash"));
        assert!(!json.contains("text"));
        assert_eq!(
            serde_json::from_str::<PendingRebootReceipt>(&json).unwrap(),
            receipt()
        );
    }

    #[test]
    fn validates_target_and_message_constraints() {
        assert!(PendingRebootReceipt::new(ReceiptTarget::SelfUser, 1, NOW).is_ok());
        assert!(
            PendingRebootReceipt::new(
                ReceiptTarget::User {
                    id: 1,
                    access_hash: 2
                },
                1,
                NOW
            )
            .is_ok()
        );
        assert!(PendingRebootReceipt::new(ReceiptTarget::Chat { id: 1 }, 1, NOW).is_ok());
        assert!(matches!(
            PendingRebootReceipt::new(ReceiptTarget::Chat { id: 0 }, 1, NOW),
            Err(ReceiptValidationError::InvalidTargetId)
        ));
        assert!(
            PendingRebootReceipt::new(
                ReceiptTarget::Channel {
                    id: 1,
                    access_hash: 0
                },
                1,
                NOW
            )
            .is_ok()
        );
        assert!(matches!(
            PendingRebootReceipt::new(ReceiptTarget::SelfUser, 0, NOW),
            Err(ReceiptValidationError::InvalidMessageId)
        ));
    }

    #[test]
    fn elapsed_saturates_small_future_skew_and_rejects_large_future_skew() {
        assert_eq!(receipt().elapsed_millis(NOW - 1).unwrap(), 0);
        assert!(matches!(
            receipt().elapsed_millis(NOW - MAX_FUTURE_MILLIS - 1),
            Err(ReceiptValidationError::TooFarInFuture)
        ));
    }

    #[tokio::test]
    async fn arms_loads_and_removes_with_private_permissions() {
        let path = path();
        let store = RebootReceiptStore::new(path.clone());
        assert_eq!(store.arm(receipt()).await.unwrap(), ArmOutcome::Armed);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(matches!(
            store.load_at(NOW + 5).await.unwrap(),
            LoadOutcome::Pending(LoadedReceipt {
                elapsed_millis: 5,
                ..
            })
        ));
        assert!(store.remove().await.unwrap());
        assert_eq!(store.load_at(NOW).await.unwrap(), LoadOutcome::Absent);
        fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn coordinator_retries_temporary_failure_then_deletes_on_success() {
        let path = path();
        let store = RebootReceiptStore::new(path.clone());
        store.arm(receipt()).await.unwrap();
        let clock = FakeClock::new([NOW, NOW + 1, NOW + 2]);
        let mut editor =
            FakeEditor::new([ReceiptEditOutcome::Temporary, ReceiptEditOutcome::Applied]);
        let mut sleeper = FakeSleeper::default();

        assert_eq!(
            complete_pending_reboot_receipt(
                &store,
                &clock,
                &mut editor,
                &mut sleeper,
                RebootReceiptRetryPolicy::default(),
                Locale::Russian,
            )
            .await
            .unwrap(),
            RebootReceiptCompletion::Applied
        );
        assert!(!path.exists());
        assert_eq!(editor.intents.len(), 2);
        assert_eq!(sleeper.0, vec![REBOOT_RECEIPT_RETRY_BACKOFF]);
        fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn coordinator_retains_receipt_after_temporary_exhaustion() {
        let path = path();
        let store = RebootReceiptStore::new(path.clone());
        store.arm(receipt()).await.unwrap();
        let clock = FakeClock::new([NOW, NOW + 1, NOW + 2, NOW + 3]);
        let mut editor = FakeEditor::new([ReceiptEditOutcome::Temporary; 3]);
        let mut sleeper = FakeSleeper::default();

        assert_eq!(
            complete_pending_reboot_receipt(
                &store,
                &clock,
                &mut editor,
                &mut sleeper,
                RebootReceiptRetryPolicy::default(),
                Locale::Russian,
            )
            .await
            .unwrap(),
            RebootReceiptCompletion::TemporaryExhausted
        );
        assert!(path.exists());
        assert_eq!(editor.intents.len(), 3);
        assert_eq!(sleeper.0.len(), 2);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn coordinator_deletes_terminal_and_already_applied_receipts() {
        for outcome in [
            ReceiptEditOutcome::Terminal,
            ReceiptEditOutcome::AlreadyApplied,
        ] {
            let path = path();
            let store = RebootReceiptStore::new(path.clone());
            store.arm(receipt()).await.unwrap();
            let clock = FakeClock::new([NOW, NOW + 1]);
            let mut editor = FakeEditor::new([outcome]);
            let mut sleeper = FakeSleeper::default();

            let result = complete_pending_reboot_receipt(
                &store,
                &clock,
                &mut editor,
                &mut sleeper,
                RebootReceiptRetryPolicy::default(),
                Locale::Russian,
            )
            .await
            .unwrap();
            assert!(matches!(
                result,
                RebootReceiptCompletion::Terminal | RebootReceiptCompletion::AlreadyApplied
            ));
            assert!(!path.exists());
            fs::remove_dir(path.parent().unwrap()).unwrap();
        }
    }

    #[tokio::test]
    async fn coordinator_recomputes_elapsed_and_formats_exact_completion_text() {
        let path = path();
        let store = RebootReceiptStore::new(path.clone());
        store.arm(receipt()).await.unwrap();
        let clock = FakeClock::new([NOW + 5, NOW + 10, NOW + 25]);
        let mut editor =
            FakeEditor::new([ReceiptEditOutcome::Temporary, ReceiptEditOutcome::Applied]);
        let mut sleeper = FakeSleeper::default();

        complete_pending_reboot_receipt(
            &store,
            &clock,
            &mut editor,
            &mut sleeper,
            RebootReceiptRetryPolicy::default(),
            Locale::Russian,
        )
        .await
        .unwrap();
        assert_eq!(
            editor.intents[0].text,
            "✅ Lavis перезагрузился\n\nВремя перезагрузки: 0 с"
        );
        assert_eq!(
            editor.intents[1].text,
            "✅ Lavis перезагрузился\n\nВремя перезагрузки: 0 с"
        );
        fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn existing_pending_receipt_conflicts_without_replacement() {
        let path = path();
        let store = RebootReceiptStore::new(path.clone());
        store.arm(receipt()).await.unwrap();
        let next = PendingRebootReceipt::new(ReceiptTarget::SelfUser, 100, NOW).unwrap();
        assert_eq!(store.arm(next).await.unwrap(), ArmOutcome::Conflict);
        assert_eq!(fs::read(&path).unwrap(), serialize(&receipt()).unwrap());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn expires_and_discards_receipt() {
        let path = path();
        let store = RebootReceiptStore::new(path.clone());
        store.arm(receipt()).await.unwrap();
        assert!(matches!(
            store.load_at(NOW + RECEIPT_TTL_MILLIS).await.unwrap(),
            LoadOutcome::Pending(_)
        ));
        assert_eq!(
            store.load_at(NOW + RECEIPT_TTL_MILLIS + 1).await.unwrap(),
            LoadOutcome::Discarded(DiscardReason::Expired)
        );
        assert!(!path.exists());
        fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn future_receipt_is_discarded() {
        let path = path();
        let store = RebootReceiptStore::new(path.clone());
        let future =
            PendingRebootReceipt::new(ReceiptTarget::SelfUser, 1, NOW + MAX_FUTURE_MILLIS + 1)
                .unwrap();
        store.arm(future).await.unwrap();
        assert_eq!(
            store.load_at(NOW).await.unwrap(),
            LoadOutcome::Discarded(DiscardReason::Invalid(
                ReceiptValidationError::TooFarInFuture
            ))
        );
        fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn corrupt_and_unknown_versions_are_discarded() {
        let path = path();
        let store = RebootReceiptStore::new(path.clone());
        fs::write(&path, b"{").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            store.load_at(NOW).await.unwrap(),
            LoadOutcome::Discarded(DiscardReason::Malformed)
        );
        fs::write(
            &path,
            br#"{"version":2,"target":{"kind":"self_user"},"message_id":1,"started_unix_ms":1}"#,
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            store.load_at(NOW).await.unwrap(),
            LoadOutcome::Discarded(DiscardReason::UnsupportedVersion(2))
        );
        fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn oversized_receipt_is_discarded_instead_of_blocking_future_reboots() {
        let path = path();
        fs::write(&path, vec![b'x'; MAX_RECEIPT_BYTES + 1]).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            RebootReceiptStore::new(path.clone())
                .load_at(NOW)
                .await
                .unwrap(),
            LoadOutcome::Discarded(DiscardReason::UnsafeState)
        );
        assert!(!path.exists());
        fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn invalid_expired_receipt_may_be_rearmed() {
        let path = path();
        let store = RebootReceiptStore::new(path.clone());
        store.arm(receipt()).await.unwrap();
        let _ = store.load_at(NOW + RECEIPT_TTL_MILLIS + 1).await.unwrap();
        assert_eq!(store.arm(receipt()).await.unwrap(), ArmOutcome::Armed);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn removes_only_exact_stale_temporary_files() {
        let path = path();
        let parent = path.parent().unwrap();
        let stale = parent.join(".reboot-receipt-0123456789abcdef0123456789abcdef.tmp");
        let unrelated = parent.join(".reboot-receipt-not-a-nonce.tmp");
        fs::write(&stale, "stale").unwrap();
        fs::write(&unrelated, "keep").unwrap();
        let cleanup = RebootReceiptStore::new(path.clone())
            .cleanup_stale_temporaries()
            .await
            .unwrap();
        assert_eq!(cleanup.removed, 1);
        assert_eq!(cleanup.failures, 0);
        assert!(!stale.exists());
        assert!(unrelated.exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unsafe_receipt_file_is_discarded_and_symlink_is_not_followed() {
        use std::os::unix::fs::symlink;
        let path = path();
        let parent = path.parent().unwrap();
        let target = parent.join("outside");
        fs::write(&target, "outside").unwrap();
        symlink(&target, &path).unwrap();
        let outcome = RebootReceiptStore::new(path.clone())
            .load_at(NOW)
            .await
            .unwrap();
        assert_eq!(outcome, LoadOutcome::Discarded(DiscardReason::UnsafeState));
        assert_eq!(fs::read_to_string(target).unwrap(), "outside");
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn new_state_directory_is_created_with_private_mode_and_symlink_directory_is_rejected() {
        use std::os::unix::fs::symlink;
        let base = path();
        let root = base.parent().unwrap().to_path_buf();
        let nested = root.join("new-state").join("reboot-receipt.json");
        RebootReceiptStore::new(nested.clone())
            .arm(receipt())
            .await
            .unwrap();
        assert_eq!(
            fs::metadata(nested.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let linked = root.join("linked-state");
        symlink(nested.parent().unwrap(), &linked).unwrap();
        assert!(matches!(
            RebootReceiptStore::new(linked.join("reboot-receipt.json"))
                .load_at(NOW)
                .await,
            Err(ReceiptStoreError::UnsafeState { .. })
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn insecure_receipt_file_is_discarded_and_insecure_directory_is_rejected() {
        let path = path();
        let store = RebootReceiptStore::new(path.clone());
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            store.load_at(NOW).await.unwrap(),
            LoadOutcome::Discarded(DiscardReason::UnsafeState)
        );
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            store.load_at(NOW).await,
            Err(ReceiptStoreError::UnsafeState { .. })
        ));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
