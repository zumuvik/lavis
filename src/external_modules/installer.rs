//! Filesystem-only external module installation.
//!
//! `PendingInspection`, `RedeemedInspection`, and `ValidatedStage` are owned
//! by the inspection flow.  The integration lane must expose a
//! `ValidatedStage` as the private wrapper and child module directory used by
//! [`install_staged_module`]; this module intentionally neither redeems nor
//! re-inspects a pending stage.

use rustix::{
    fs::{CWD, RenameFlags},
    io::Errno,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

use super::manifest::{ExternalModuleDescriptor, validate_manifest_at, validate_module_id};
use crate::error::ExternalError;

const OWNER_FILE: &str = "owner.json";
const STAGE_CHILD: &str = "payload";
const WRAPPER_PREFIX: &str = ".lmod-install-";
const BACKUP_PREFIX: &str = ".lmod-backup-";
/// Backup names embed 8 random bytes as hex so concurrent updates for the same
/// module id can never collide on a backup directory name.
const BACKUP_NONCE_HEX_LEN: usize = 16;

/// Provenance bound to an installation wrapper.  Strict decoding prevents an
/// abandoned-wrapper cleanup from accepting a file with an ambiguous schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StageMarker {
    pub format: u32,
    pub created_by: String,
    pub created_at: u64,
}

#[derive(Debug, Error)]
pub(crate) enum InstallError {
    #[error("module id is invalid")]
    InvalidModuleId,
    #[error("staging wrapper is unsafe or incomplete")]
    UnsafeStage,
    #[error("staging marker is malformed")]
    InvalidMarker,
    #[error("installation root is unsafe")]
    UnsafeInstallRoot,
    #[error("module target already exists")]
    TargetExists,
    #[error("staging and install root are on different filesystems")]
    CrossDevice,
    #[error("filesystem operation failed")]
    Filesystem(#[source] io::Error),
    #[error("installed module manifest did not validate")]
    PostInstallValidationFailed {
        #[source]
        validation: ExternalError,
    },
    #[error("installed module manifest did not validate and target rollback failed")]
    TargetCleanup(#[source] TargetCleanupError),
}

/// A rollback failure for a target that was already made visible. This is
/// distinct from [`StageCleanupError`], which concerns abandoned private staging.
#[derive(Debug, Error)]
#[error("target rollback failed ({kind:?})")]
pub(crate) struct TargetCleanupError {
    #[source]
    pub validation: ExternalError,
    pub kind: io::ErrorKind,
}

/// A successfully installed module and the descriptor validated after its
/// no-replace rename. The runtime must use this descriptor directly.
#[derive(Debug)]
pub(crate) struct InstalledModule {
    pub(crate) descriptor: super::manifest::ExternalModuleDescriptor,
}

/// The only marker schema accepted during abandoned-wrapper cleanup.
pub(crate) const STAGE_FORMAT: u32 = 1;
pub(crate) const STAGE_CREATED_BY: &str = "lavis";

/// Create a strict ownership marker in a private staging wrapper.
pub(crate) fn write_stage_marker(
    wrapper: &Path,
    created_at: SystemTime,
) -> Result<(), InstallError> {
    if !is_plain_directory(wrapper)? {
        return Err(InstallError::UnsafeStage);
    }
    let marker = StageMarker {
        format: STAGE_FORMAT,
        created_by: STAGE_CREATED_BY.into(),
        created_at: created_at
            .duration_since(UNIX_EPOCH)
            .map_err(|_| InstallError::InvalidMarker)?
            .as_secs(),
    };
    let bytes = serde_json::to_vec(&marker).map_err(|_| InstallError::InvalidMarker)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(wrapper.join(OWNER_FILE))
        .map_err(InstallError::Filesystem)?;
    file.write_all(&bytes).map_err(InstallError::Filesystem)
}

/// Remove wrappers left after an interrupted install, but only when their
/// strict owner marker proves they are installer-owned.  Symlinks are never
/// traversed or removed through this routine.
pub(crate) fn cleanup_abandoned_wrappers(
    staging_root: &Path,
) -> Result<Vec<StageCleanupError>, InstallError> {
    if !is_plain_directory(staging_root)? {
        return Err(InstallError::UnsafeStage);
    }
    let mut failures = Vec::new();
    for entry in fs::read_dir(staging_root).map_err(InstallError::Filesystem)? {
        let entry = entry.map_err(InstallError::Filesystem)?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(WRAPPER_PREFIX) {
            continue;
        }
        let wrapper = entry.path();
        if !is_plain_directory(&wrapper)? || read_stage_marker(&wrapper).is_err() {
            continue;
        }
        if let Err(error) = remove_marked_wrapper_no_follow(&wrapper) {
            failures.push(StageCleanupError {
                wrapper,
                kind: error.kind(),
            });
        }
    }
    Ok(failures)
}

/// Cleanup failure after a wrapper was identified as installer-owned. It is
/// distinct from `TargetCleanupError`, which follows a visible install failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StageCleanupError {
    pub wrapper: std::path::PathBuf,
    pub kind: io::ErrorKind,
}

/// Atomically install the `payload` child of an inspection-owned wrapper.
///
/// The caller must obtain `wrapper` and its owner from `ValidatedStage`, after
/// consuming a `RedeemedInspection`.  `PendingInspection` must never reach
/// this function.  The final validation deliberately runs after the rename;
/// if it fails, the newly created target is removed.  A rollback failure is a
/// distinct error because it leaves a visible target behind.
pub(crate) fn install_staged_module(
    wrapper: &Path,
    install_root: &Path,
    module_id: &str,
) -> Result<InstalledModule, InstallError> {
    validate_module_id(module_id).map_err(|_| InstallError::InvalidModuleId)?;
    if !is_plain_directory(wrapper)? {
        return Err(InstallError::UnsafeStage);
    }
    if !is_plain_directory(install_root)? {
        return Err(InstallError::UnsafeInstallRoot);
    }
    read_stage_marker(wrapper)?;
    let source = wrapper.join(STAGE_CHILD);
    if !is_plain_directory(&source)? {
        return Err(InstallError::UnsafeStage);
    }
    let target = install_root.join(module_id);
    match rustix::fs::renameat_with(CWD, &source, CWD, &target, RenameFlags::NOREPLACE) {
        Ok(()) => {}
        Err(error) => return Err(map_rename_error(error)),
    }

    let descriptor = match validate_manifest_at(&target.join("module.json"), Some(module_id)) {
        Ok(descriptor) => descriptor,
        Err(validation) => {
            return match remove_tree_no_follow(&target) {
                Ok(()) => Err(InstallError::PostInstallValidationFailed { validation }),
                Err(error) => Err(InstallError::TargetCleanup(TargetCleanupError {
                    validation,
                    kind: error.kind(),
                })),
            };
        }
    };

    // After the child has moved, the wrapper contains only owner.json.  Its
    // cleanup is best effort: a successful atomic install remains successful.
    if let Err(error) = remove_empty_wrapper(wrapper) {
        tracing::warn!(
            event = "external_module_install_wrapper_cleanup_failed",
            error = %error,
            "Installed external module successfully but could not remove its empty staging wrapper"
        );
    }
    Ok(InstalledModule { descriptor })
}

/// A successfully updated module plus the backup of the replaced generation.
/// The caller owns backup retention via [`prune_module_backups`].
#[derive(Debug)]
pub(crate) struct UpdatedModule {
    pub(crate) descriptor: ExternalModuleDescriptor,
    pub(crate) backup_path: PathBuf,
}

#[derive(Debug, Error)]
pub(crate) enum UpdateError {
    #[error("module id is invalid")]
    InvalidModuleId,
    #[error("staging wrapper is unsafe or incomplete")]
    UnsafeStage,
    #[error("staging marker is malformed")]
    InvalidMarker,
    #[error("installation root is unsafe")]
    UnsafeInstallRoot,
    #[error("backup root is unsafe")]
    UnsafeBackupsRoot,
    #[error("filesystem operation failed")]
    Filesystem(#[source] io::Error),
    #[error("updated module manifest did not validate; previous generation restored")]
    ValidationFailed(#[source] ExternalError),
    #[error("update failed and the previous generation could not be restored")]
    RestoreFailed(#[source] io::Error),
}

impl UpdateError {
    /// Static, path-free reason safe to surface in user-facing output.
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::InvalidModuleId => "invalid module id",
            Self::UnsafeStage | Self::InvalidMarker => "staging wrapper was rejected",
            Self::UnsafeInstallRoot | Self::UnsafeBackupsRoot => {
                "module storage rejected the update"
            }
            Self::Filesystem(_) => "filesystem error",
            Self::ValidationFailed(_) => "final validation failed",
            Self::RestoreFailed(_) => "update failed; manual cleanup required",
        }
    }
}

fn map_stage_error(error: InstallError) -> UpdateError {
    match error {
        InstallError::InvalidModuleId => UpdateError::InvalidModuleId,
        InstallError::UnsafeStage => UpdateError::UnsafeStage,
        InstallError::InvalidMarker => UpdateError::InvalidMarker,
        InstallError::UnsafeInstallRoot => UpdateError::UnsafeInstallRoot,
        other => UpdateError::Filesystem(io::Error::other(other.to_string())),
    }
}

fn backup_dir_name(module_id: &str, nonce_hex: &str) -> String {
    format!("{BACKUP_PREFIX}{module_id}-{nonce_hex}")
}

fn fresh_backup_nonce() -> Result<String, UpdateError> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|error| UpdateError::Filesystem(io::Error::other(error.to_string())))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Splits `.lmod-backup-<id>-<nonce>` into its module id and nonce. Returns
/// `None` for names that do not parse or embed an invalid module id; such
/// directories are residue and must never be restored over a module target.
fn parse_backup_name(name: &str) -> Option<(&str, &str)> {
    let remainder = name.strip_prefix(BACKUP_PREFIX)?;
    let (id, nonce) = remainder.rsplit_once('-')?;
    if nonce.len() != BACKUP_NONCE_HEX_LEN || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    validate_module_id(id).ok()?;
    Some((id, nonce))
}

/// Atomically replaces an installed module with the staged payload.
///
/// The previous generation is renamed into a fresh backup directory first; a
/// missing target falls back to the plain no-replace install path. If the new
/// payload fails to rename or validate, the backup is renamed back (the old
/// generation is guaranteed to be visible again) and the original failure is
/// reported. A failed restore is a distinct error because it leaves residue.
pub(crate) fn update_staged_module(
    wrapper: &Path,
    install_root: &Path,
    backups_root: &Path,
    module_id: &str,
) -> Result<UpdatedModule, UpdateError> {
    validate_module_id(module_id).map_err(|_| UpdateError::InvalidModuleId)?;
    if !is_plain_directory(wrapper).map_err(map_stage_error)? {
        return Err(UpdateError::UnsafeStage);
    }
    read_stage_marker(wrapper).map_err(map_stage_error)?;
    if !is_plain_directory(install_root).map_err(map_stage_error)? {
        return Err(UpdateError::UnsafeInstallRoot);
    }
    fs::create_dir_all(backups_root).map_err(UpdateError::Filesystem)?;
    if !is_plain_directory(backups_root).map_err(map_stage_error)? {
        return Err(UpdateError::UnsafeBackupsRoot);
    }
    let source = wrapper.join(STAGE_CHILD);
    if !is_plain_directory(&source).map_err(map_stage_error)? {
        return Err(UpdateError::UnsafeStage);
    }
    let target = install_root.join(module_id);
    let backup = backups_root.join(backup_dir_name(module_id, &fresh_backup_nonce()?));
    let moved_old = match rustix::fs::renameat(CWD, &target, CWD, &backup) {
        Ok(()) => true,
        Err(Errno::NOENT) => false,
        Err(error) => {
            return Err(UpdateError::Filesystem(io::Error::from_raw_os_error(
                error.raw_os_error(),
            )));
        }
    };

    // A plain rename onto the target can fail with ENOTEMPTY while the new
    // generation still occupies it, so the new content is removed first.
    let restore_old = |cause: io::Error| -> UpdateError {
        let _ = remove_tree_no_follow(&target);
        if !moved_old {
            return UpdateError::Filesystem(cause);
        }
        match rustix::fs::renameat(CWD, &backup, CWD, &target) {
            Ok(()) => UpdateError::Filesystem(cause),
            Err(error) => {
                UpdateError::RestoreFailed(io::Error::from_raw_os_error(error.raw_os_error()))
            }
        }
    };

    match rustix::fs::renameat_with(CWD, &source, CWD, &target, RenameFlags::NOREPLACE) {
        Ok(()) => {}
        Err(error) => {
            return Err(restore_old(io::Error::from_raw_os_error(
                error.raw_os_error(),
            )));
        }
    }

    let descriptor = match validate_manifest_at(&target.join("module.json"), Some(module_id)) {
        Ok(descriptor) => descriptor,
        Err(validation) => {
            if !moved_old {
                return match remove_tree_no_follow(&target) {
                    Ok(()) => Err(UpdateError::ValidationFailed(validation)),
                    Err(error) => Err(UpdateError::RestoreFailed(error)),
                };
            }
            // The new generation must be removed before the backup can move
            // back over it.
            if let Err(error) = remove_tree_no_follow(&target) {
                return Err(UpdateError::RestoreFailed(error));
            }
            return match rustix::fs::renameat(CWD, &backup, CWD, &target) {
                Ok(()) => Err(UpdateError::ValidationFailed(validation)),
                Err(error) => Err(UpdateError::RestoreFailed(io::Error::from_raw_os_error(
                    error.raw_os_error(),
                ))),
            };
        }
    };

    // Same best-effort wrapper cleanup as a fresh install.
    if let Err(error) = remove_empty_wrapper(wrapper) {
        tracing::warn!(
            event = "external_module_update_wrapper_cleanup_failed",
            error = %error,
            "Updated external module successfully but could not remove its empty staging wrapper"
        );
    }
    Ok(UpdatedModule {
        descriptor,
        backup_path: backup,
    })
}

/// Deletes every other backup for `module_id`, keeping `keep`. Best effort:
/// pruning failures never invalidate the completed update.
pub(crate) fn prune_module_backups(backups_root: &Path, module_id: &str, keep: &Path) {
    let Ok(entries) = fs::read_dir(backups_root) else {
        return;
    };
    let prefix = format!("{BACKUP_PREFIX}{module_id}-");
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        if let Err(error) = remove_tree_no_follow(&path) {
            tracing::warn!(
                event = "external_module_backup_prune_failed",
                error = %error,
                backup = %path.display(),
                "Could not prune an old external module backup"
            );
        }
    }
}

/// Boot-time reconciliation of leftover backups.
///
/// For each backup group: if the module target is missing, the newest backup is
/// restored (an interrupted update is rolled forward to the old generation) and
/// the remaining backups are deleted; if the target exists, all backups for the
/// id are deleted. Unparsable backup names are residue and are removed. Actions
/// are logged without payload contents.
pub(crate) fn reconcile_module_backups(install_root: &Path, backups_root: &Path) {
    let Ok(entries) = fs::read_dir(backups_root) else {
        return;
    };
    let mut groups: BTreeMap<String, Vec<(SystemTime, String, PathBuf)>> = BTreeMap::new();
    for entry in entries.flatten() {
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some((id, _nonce)) = parse_backup_name(name) {
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            groups.entry(id.to_owned()).or_default().push((
                modified,
                name.to_owned(),
                entry.path(),
            ));
            continue;
        }
        // Unparsable names can never be identified as a rollback generation.
        if name.starts_with(BACKUP_PREFIX) && remove_tree_no_follow(&entry.path()).is_err() {
            tracing::warn!(
                event = "external_module_backup_reconcile_failed",
                backup = %entry.path().display(),
                "Could not remove an unparsable external module backup"
            );
        }
    }
    for (id, mut backups) in groups {
        backups.sort_by_key(|(modified, _, _)| *modified);
        if install_root.join(&id).is_dir() {
            for (_, _, path) in &backups {
                let _ = remove_tree_no_follow(path);
            }
            continue;
        }
        let Some((_, _, newest)) = backups.pop() else {
            continue;
        };
        match rustix::fs::renameat(CWD, &newest, CWD, install_root.join(&id)) {
            Ok(()) => {
                tracing::info!(
                    event = "external_module_backup_restored",
                    module_id = %id,
                    "Restored an external module from its newest backup"
                );
                for (_, _, path) in &backups {
                    let _ = remove_tree_no_follow(path);
                }
            }
            Err(error) => {
                // Keep every backup so the next boot can retry the restore.
                tracing::warn!(
                    event = "external_module_backup_restore_failed",
                    error = %io::Error::from_raw_os_error(error.raw_os_error()),
                    module_id = %id,
                    "Could not restore an external module backup"
                );
            }
        }
    }
}

/// Removes only a redeemed wrapper. Other live approvals are never touched.
pub(crate) fn cleanup_redeemed_stage(wrapper: &Path) -> Result<(), StageCleanupError> {
    if !matches!(is_plain_directory(wrapper), Ok(true)) || read_stage_marker(wrapper).is_err() {
        return Err(StageCleanupError {
            wrapper: wrapper.to_path_buf(),
            kind: io::ErrorKind::InvalidData,
        });
    }
    remove_marked_wrapper_no_follow(wrapper).map_err(|error| StageCleanupError {
        wrapper: wrapper.to_path_buf(),
        kind: error.kind(),
    })
}

fn map_rename_error(error: Errno) -> InstallError {
    match error {
        Errno::EXIST => InstallError::TargetExists,
        Errno::XDEV => InstallError::CrossDevice,
        error => InstallError::Filesystem(io::Error::from_raw_os_error(error.raw_os_error())),
    }
}

fn read_stage_marker(wrapper: &Path) -> Result<StageMarker, InstallError> {
    let owner_path = wrapper.join(OWNER_FILE);
    let metadata = fs::symlink_metadata(&owner_path).map_err(|_| InstallError::InvalidMarker)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 4096 {
        return Err(InstallError::InvalidMarker);
    }
    let bytes = fs::read(owner_path).map_err(|_| InstallError::InvalidMarker)?;
    let marker =
        serde_json::from_slice::<StageMarker>(&bytes).map_err(|_| InstallError::InvalidMarker)?;
    if marker.format != STAGE_FORMAT || marker.created_by != STAGE_CREATED_BY {
        return Err(InstallError::InvalidMarker);
    }
    Ok(marker)
}

fn is_plain_directory(path: &Path) -> Result<bool, InstallError> {
    let metadata = fs::symlink_metadata(path).map_err(InstallError::Filesystem)?;
    Ok(metadata.file_type().is_dir() && !metadata.file_type().is_symlink())
}

fn remove_empty_wrapper(wrapper: &Path) -> io::Result<()> {
    let owner = wrapper.join(OWNER_FILE);
    let metadata = fs::symlink_metadata(&owner)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "owner symlink"));
    }
    let marker = fs::read(&owner)?;
    fs::remove_file(owner)?;
    match fs::remove_dir(wrapper) {
        Ok(()) => Ok(()),
        Err(remove_error) => {
            // Restore the exact marker before reporting failure. Startup cleanup
            // can then identify this wrapper even when a concurrent or stray
            // file prevented removing the directory.
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(wrapper.join(OWNER_FILE))
            {
                Ok(mut file) => match file.write_all(&marker) {
                    Ok(()) => Err(remove_error),
                    Err(restore_error) => Err(restore_error),
                },
                Err(restore_error) => Err(restore_error),
            }
        }
    }
}

/// Remove a known installer wrapper while preserving `owner.json` until every
/// payload child has been removed. A failure before the final two operations
/// leaves the marker intact for startup cleanup.
fn remove_marked_wrapper_no_follow(wrapper: &Path) -> io::Result<()> {
    for entry in fs::read_dir(wrapper)? {
        let entry = entry?;
        if entry.file_name() == OWNER_FILE {
            continue;
        }
        remove_tree_no_follow(&entry.path())?;
    }
    remove_empty_wrapper(wrapper)
}

fn remove_tree_no_follow(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "symlink in removal path",
        ));
    }
    if metadata.is_file() {
        return fs::remove_file(path);
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported removal type",
        ));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let child_metadata = fs::symlink_metadata(&child)?;
        if child_metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "symlink in removal tree",
            ));
        }
        remove_tree_no_follow(&child)?;
    }
    fs::remove_dir(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn root(label: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("lavis-installer-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn wrapper(root: &Path, invalid_manifest: bool) -> std::path::PathBuf {
        let wrapper = root.join(".lmod-install-test");
        fs::create_dir(&wrapper).unwrap();
        write_stage_marker(&wrapper, SystemTime::UNIX_EPOCH).unwrap();
        let payload = wrapper.join(STAGE_CHILD);
        fs::create_dir(&payload).unwrap();
        let manifest: &[u8] = if invalid_manifest {
            b"{}"
        } else {
            b"{\"schema_version\":2,\"id\":\"test\",\"name\":\"Test\",\"version\":\"1\",\"author\":\"A\",\"entrypoint\":\"run\",\"commands\":[{\"name\":\"go\",\"summary_ru\":\"x\",\"description_ru\":\"x\",\"usage\":\"<value>\"}]}"
        };
        fs::write(payload.join("module.json"), manifest).unwrap();
        fs::write(payload.join("run"), b"#!/bin/sh").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(payload.join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        wrapper
    }

    #[test]
    fn owner_schema_rejects_unknown_fields() {
        let parsed = serde_json::from_slice::<StageMarker>(
            br#"{"format":1,"created_by":"lavis","created_at":1,"extra":true}"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn no_replace_rename_errors_keep_their_semantics() {
        assert!(matches!(
            map_rename_error(Errno::EXIST),
            InstallError::TargetExists
        ));
        assert!(matches!(
            map_rename_error(Errno::XDEV),
            InstallError::CrossDevice
        ));
    }

    #[test]
    fn failed_empty_wrapper_removal_restores_its_marker() {
        let wrapper = std::env::temp_dir().join(format!(
            "lavis-installer-wrapper-cleanup-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&wrapper);
        fs::create_dir(&wrapper).unwrap();
        write_stage_marker(&wrapper, SystemTime::UNIX_EPOCH).unwrap();
        fs::write(wrapper.join("prevents-removal"), b"x").unwrap();

        assert!(remove_empty_wrapper(&wrapper).is_err());
        assert!(wrapper.join(OWNER_FILE).is_file());
        assert!(read_stage_marker(&wrapper).is_ok());

        fs::remove_dir_all(&wrapper).unwrap();
    }

    #[test]
    fn target_collision_preserves_marked_payload() {
        let root = root("collision");
        let wrapper = wrapper(&root, false);
        let install_root = root.join("installed");
        fs::create_dir(&install_root).unwrap();
        fs::create_dir(install_root.join("test")).unwrap();
        assert!(matches!(
            install_staged_module(&wrapper, &install_root, "test"),
            Err(InstallError::TargetExists)
        ));
        assert!(wrapper.join(OWNER_FILE).is_file());
        assert!(wrapper.join(STAGE_CHILD).is_dir());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn validation_failure_rolls_back_target_and_retains_marker() {
        let root = root("rollback");
        let wrapper = wrapper(&root, true);
        let install_root = root.join("installed");
        fs::create_dir(&install_root).unwrap();
        assert!(matches!(
            install_staged_module(&wrapper, &install_root, "test"),
            Err(InstallError::PostInstallValidationFailed { .. })
        ));
        assert!(!install_root.join("test").exists());
        assert!(read_stage_marker(&wrapper).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_failure_retains_validation_error() {
        let root = root("rollback-failure");
        let wrapper = wrapper(&root, true);
        symlink(
            "/does-not-matter",
            wrapper.join(STAGE_CHILD).join("blocks-cleanup"),
        )
        .unwrap();
        let install_root = root.join("installed");
        fs::create_dir(&install_root).unwrap();
        match install_staged_module(&wrapper, &install_root, "test") {
            Err(InstallError::TargetCleanup(error)) => {
                assert!(matches!(error.validation, ExternalError::MalformedManifest));
            }
            other => panic!("unexpected result: {other:?}"),
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn marker_symlinks_and_unknown_fields_are_rejected() {
        let root = root("marker");
        let wrapper = root.join(".lmod-install-marker");
        fs::create_dir(&wrapper).unwrap();
        fs::write(
            wrapper.join(OWNER_FILE),
            br#"{"format":1,"created_by":"lavis","created_at":1,"extra":true}"#,
        )
        .unwrap();
        assert!(matches!(
            read_stage_marker(&wrapper),
            Err(InstallError::InvalidMarker)
        ));
        fs::remove_file(wrapper.join(OWNER_FILE)).unwrap();
        symlink("/tmp", wrapper.join(OWNER_FILE)).unwrap();
        assert!(matches!(
            read_stage_marker(&wrapper),
            Err(InstallError::InvalidMarker)
        ));
        let _ = fs::remove_dir_all(root);
    }

    fn backup_nonce(label: &str) -> String {
        let mut bytes = label.as_bytes().to_vec();
        bytes.resize(8, b'0');
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn install_module_payload(install_root: &Path, id: &str, version: &str) {
        let module = install_root.join(id);
        fs::create_dir_all(&module).unwrap();
        fs::write(
            module.join("module.json"),
            format!(
                r#"{{"schema_version":2,"id":"{id}","name":"Test","version":"{version}","author":"A","entrypoint":"run","commands":[{{"name":"go","summary_ru":"x","description_ru":"x","usage":"<value>"}}]}}"#
            ),
        )
        .unwrap();
        fs::write(module.join("run"), b"#!/bin/sh").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(module.join("run"), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn update_swaps_generations_and_keeps_the_old_one_as_backup() {
        let root = root("update-success");
        let wrapper = wrapper(&root, false);
        let install_root = root.join("modules");
        let backups_root = root.join("module-backups");
        fs::create_dir_all(&install_root).unwrap();
        install_module_payload(&install_root, "test", "0");
        let updated = update_staged_module(&wrapper, &install_root, &backups_root, "test").unwrap();
        assert_eq!(updated.descriptor.version, "1");
        assert!(updated.backup_path.is_dir());
        assert!(
            updated
                .backup_path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(BACKUP_PREFIX)
        );
        // The new generation is in place; the old one is fully preserved.
        assert!(
            fs::read_to_string(install_root.join("test/module.json"))
                .unwrap()
                .contains("\"version\":\"1\"")
        );
        let backup_manifest = fs::read_to_string(updated.backup_path.join("module.json")).unwrap();
        assert!(backup_manifest.contains("\"version\":\"0\""));
        // The staging wrapper was consumed by a successful update.
        assert!(!wrapper.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_with_missing_target_falls_back_to_fresh_install() {
        let root = root("update-fresh");
        let wrapper = wrapper(&root, false);
        let install_root = root.join("modules");
        let backups_root = root.join("module-backups");
        fs::create_dir_all(&install_root).unwrap();
        let updated = update_staged_module(&wrapper, &install_root, &backups_root, "test").unwrap();
        assert!(install_root.join("test/module.json").is_file());
        assert!(!updated.backup_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_update_validation_restores_the_previous_generation() {
        let root = root("update-rollback");
        let wrapper = wrapper(&root, true);
        let install_root = root.join("modules");
        let backups_root = root.join("module-backups");
        fs::create_dir_all(&install_root).unwrap();
        install_module_payload(&install_root, "test", "1");
        match update_staged_module(&wrapper, &install_root, &backups_root, "test") {
            Err(UpdateError::ValidationFailed(_)) => {}
            other => panic!("unexpected result: {other:?}"),
        }
        // The old generation is back, the new payload is gone, the wrapper is
        // intact for cleanup.
        assert!(install_root.join("test/module.json").is_file());
        assert!(
            fs::read_to_string(install_root.join("test/module.json"))
                .unwrap()
                .contains("\"version\":\"1\"")
        );
        assert!(fs::read_dir(&backups_root).unwrap().next().is_none());
        assert!(wrapper.join(OWNER_FILE).is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn backup_retention_keeps_only_the_newest_generation_per_module() {
        let root = root("update-retention");
        let install_root = root.join("modules");
        let backups_root = root.join("module-backups");
        fs::create_dir_all(&install_root).unwrap();
        install_module_payload(&install_root, "test", "1");

        let first_wrapper = wrapper(&root, false);
        let first =
            update_staged_module(&first_wrapper, &install_root, &backups_root, "test").unwrap();
        install_module_payload(&install_root, "test", "2");
        let second_wrapper = wrapper(&root, false);
        let second =
            update_staged_module(&second_wrapper, &install_root, &backups_root, "test").unwrap();
        prune_module_backups(&backups_root, "test", &second.backup_path);
        assert!(second.backup_path.is_dir());
        assert!(!first.backup_path.exists());
        assert_eq!(fs::read_dir(&backups_root).unwrap().count(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reconcile_restores_a_missing_module_from_the_newest_backup() {
        let root = root("reconcile-restore");
        let install_root = root.join("modules");
        let backups_root = root.join("module-backups");
        fs::create_dir_all(&install_root).unwrap();
        let newest = backups_root.join(backup_dir_name("gone", &backup_nonce("newest")));
        let older = backups_root.join(backup_dir_name("gone", &backup_nonce("older")));
        fs::create_dir_all(&newest).unwrap();
        fs::create_dir_all(&older).unwrap();
        fs::write(newest.join("marker"), b"new").unwrap();
        fs::write(older.join("marker"), b"old").unwrap();
        // Reconciliation orders backups by directory mtime; pin both dirs so
        // "newest" is unambiguous regardless of creation order.
        for (dir, stamp) in [(&newest, 2_000_000u64), (&older, 1_000_000)] {
            let file = fs::File::open(dir).unwrap();
            file.set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(stamp))
                .unwrap();
        }

        reconcile_module_backups(&install_root, &backups_root);
        assert!(install_root.join("gone/marker").is_file());
        assert_eq!(
            fs::read_to_string(install_root.join("gone/marker")).unwrap(),
            "new"
        );
        assert_eq!(fs::read_dir(&backups_root).unwrap().count(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reconcile_deletes_backups_of_live_modules_and_unparsable_residue() {
        let root = root("reconcile-delete");
        let install_root = root.join("modules");
        let backups_root = root.join("module-backups");
        fs::create_dir_all(&install_root).unwrap();
        install_module_payload(&install_root, "test", "1");
        let live = backups_root.join(backup_dir_name("test", &backup_nonce("live")));
        let residue = backups_root.join(format!("{BACKUP_PREFIX}BAD-{}", backup_nonce("res")));
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&residue).unwrap();

        reconcile_module_backups(&install_root, &backups_root);
        assert!(!live.exists());
        assert!(!residue.exists());
        assert!(install_root.join("test/module.json").is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn backup_names_require_a_valid_id_and_fixed_hex_nonce() {
        assert!(parse_backup_name(&backup_dir_name("test", &backup_nonce("x"))).is_some());
        assert_eq!(
            parse_backup_name(&backup_dir_name("test", &backup_nonce("x")))
                .unwrap()
                .0,
            "test"
        );
        assert!(parse_backup_name(".lmod-backup-BAD-0011223344556677").is_none());
        assert!(parse_backup_name(".lmod-backup-test-short").is_none());
        assert!(parse_backup_name(".lmod-backup-test").is_none());
        assert!(parse_backup_name("unrelated").is_none());
    }
}
