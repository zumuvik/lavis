//! Durable install receipts for external modules.
//!
//! A receipt records what was installed (version, digest, source identity) so
//! `lm info` and post-mortems do not depend on the live process state. Receipts
//! are advisory data: a missing or unreadable receipt never blocks an
//! operation.

use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use super::source_inspection::{ModuleInstallPlan, SourceIdentity};

const RECEIPT_SUFFIX: &str = ".json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModuleReceipt {
    pub module_id: String,
    pub version: String,
    pub archive_digest: String,
    pub source_identity: String,
    pub capabilities: Vec<String>,
    pub telegram_methods: Vec<String>,
    pub installed_at_unix_seconds: u64,
    pub previous_version: Option<String>,
}

pub(crate) fn receipts_root(install_root: &Path) -> PathBuf {
    install_root
        .parent()
        .unwrap_or(install_root)
        .join("module-receipts")
}

pub(crate) fn receipt_path(receipts_root: &Path, module_id: &str) -> PathBuf {
    receipts_root.join(format!("{module_id}{RECEIPT_SUFFIX}"))
}

/// Builds a receipt from a validated install plan. The version is taken from
/// the installed descriptor's peer field in the plan; `previous_version` is
/// `None` for a fresh install.
pub(crate) fn receipt_from_plan(
    plan: &ModuleInstallPlan,
    previous_version: Option<String>,
    installed_at: SystemTime,
) -> Result<ModuleReceipt, io::Error> {
    let installed_at_unix_seconds = installed_at
        .duration_since(UNIX_EPOCH)
        .map_err(|error| io::Error::other(error.to_string()))?
        .as_secs();
    Ok(ModuleReceipt {
        module_id: plan.module_id.clone(),
        version: plan.module_version.clone(),
        archive_digest: plan.archive_digest.as_hex(),
        source_identity: source_identity_text(&plan.source_identity),
        capabilities: plan.capabilities.clone(),
        telegram_methods: plan.telegram_methods.clone(),
        installed_at_unix_seconds,
        previous_version,
    })
}

fn source_identity_text(identity: &SourceIdentity) -> String {
    match identity {
        SourceIdentity::Archive => "archive".to_owned(),
        SourceIdentity::PinnedRepository(repository) => {
            format!(
                "repository {} @ {}",
                repository.repository(),
                repository.revision()
            )
        }
    }
}

/// Atomic write, mirroring the external module state writer: temp file in the
/// same directory, fsync, rename.
pub(crate) fn write_receipt(receipts_root: &Path, receipt: &ModuleReceipt) -> io::Result<()> {
    fs::create_dir_all(receipts_root)?;
    let path = receipt_path(receipts_root, &receipt.module_id);
    let bytes = serde_json::to_vec_pretty(receipt)?;
    let tmp = receipts_root.join(format!(
        ".{}.{}.{}.tmp",
        receipt.module_id,
        std::process::id(),
        receipt.installed_at_unix_seconds
    ));
    let write = |file: &mut fs::File| -> io::Result<()> {
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()
    };
    let result = (|| -> io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        write(&mut file)
    })();
    match result {
        Ok(()) => fs::rename(&tmp, &path),
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(error)
        }
    }
}

pub(crate) fn read_receipt(
    receipts_root: &Path,
    module_id: &str,
) -> io::Result<Option<ModuleReceipt>> {
    let bytes = match fs::read(receipt_path(receipts_root, module_id)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> ModuleInstallPlan {
        use super::super::source_inspection::{
            ArchiveDigest, ArchiveStatistics, InspectionTimes, SourceKind,
        };
        ModuleInstallPlan {
            source_kind: SourceKind::Archive,
            source_identity: SourceIdentity::Archive,
            module_id: "echo".to_owned(),
            module_version: "1.2.3".to_owned(),
            protocol_version: 6,
            contract_revision: None,
            entrypoint: "run".to_owned(),
            default_command: None,
            archive_digest: ArchiveDigest::from_hex(&"0".repeat(64)).unwrap(),
            archive: ArchiveStatistics {
                archive_bytes: 1,
                file_count: 1,
                compressed_bytes: 1,
                expanded_bytes: 1,
            },
            warnings: vec![],
            times: InspectionTimes {
                inspected_unix_seconds: 0,
                expires_unix_seconds: 0,
            },
            capabilities: vec!["message.send".to_owned()],
            subscriptions: vec![],
            telegram_methods: vec!["sendMessage".to_owned()],
            actions: vec![],
            fingerprint: "fp".to_owned(),
        }
    }

    #[test]
    fn receipt_roundtrips_through_disk() {
        let root = std::env::temp_dir().join(format!(
            "lavis-receipts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let receipt = receipt_from_plan(&plan(), Some("1.0.0".to_owned()), UNIX_EPOCH).unwrap();
        write_receipt(&root, &receipt).unwrap();
        let loaded = read_receipt(&root, "echo").unwrap().unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(loaded.version, "1.2.3");
        assert_eq!(loaded.previous_version.as_deref(), Some("1.0.0"));
        assert_eq!(loaded.capabilities, vec!["message.send".to_owned()]);
        // A missing receipt is not an error.
        assert!(read_receipt(&root, "missing").unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn archive_sources_render_as_a_fixed_label() {
        let receipt = receipt_from_plan(&plan(), None, UNIX_EPOCH).unwrap();
        assert_eq!(receipt.source_identity, "archive");
    }
}
