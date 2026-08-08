use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::error::SettingsError;

pub const DEFAULT_PREFIX: &str = ",";
const VERSION: u32 = 1;
const MAX_FILE_BYTES: usize = 4096;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    version: u32,
    prefix: String,
}

pub struct SettingsStore {
    path: PathBuf,
    prefix: String,
    temporary_counter: u64,
}

impl SettingsStore {
    pub async fn load(path: PathBuf) -> Result<Self, SettingsError> {
        let prefix = tokio::task::spawn_blocking({
            let path = path.clone();
            move || load_prefix(&path)
        })
        .await
        .map_err(|_| SettingsError::StorageTask)??;
        Ok(Self {
            path,
            prefix,
            temporary_counter: 0,
        })
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub async fn set_prefix(&mut self, prefix: String) -> Result<(), SettingsError> {
        validate_prefix(&prefix)?;
        self.temporary_counter = self.temporary_counter.saturating_add(1);
        let bytes = serde_json::to_vec_pretty(&SettingsFile {
            version: VERSION,
            prefix: prefix.clone(),
        })
        .map_err(|_| SettingsError::WriteTemporary)?;
        let directory_sync_error = tokio::task::spawn_blocking({
            let path = self.path.clone();
            let temporary_counter = self.temporary_counter;
            move || write_settings(&path, &bytes, temporary_counter)
        })
        .await
        .map_err(|_| SettingsError::StorageTask)??;
        self.prefix = prefix;
        if let Some(error) = directory_sync_error {
            tracing::warn!(
                event = "settings_directory_sync_failed",
                error = %error,
                "Settings file was committed but directory synchronization failed"
            );
        }
        Ok(())
    }
}

pub fn validate_prefix(prefix: &str) -> Result<(), SettingsError> {
    if !(1..=4).contains(&prefix.chars().count())
        || prefix.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || character.is_alphabetic()
                || is_invisible_format_control(character)
        })
    {
        return Err(SettingsError::InvalidPrefix);
    }
    Ok(())
}

fn is_invisible_format_control(character: char) -> bool {
    // Reject Unicode Default_Ignorable_Code_Point values, including variation
    // selectors. This intentionally rejects emoji presentation sequences such as
    // "⚙️" as prefixes; ordinary visible emoji such as "🦀" remain valid.
    matches!(character,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}'..='\u{1160}' |
        '\u{17B4}'..='\u{17B5}' | '\u{180B}'..='\u{180F}' |
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' |
        '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' |
        '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFF8}' |
        '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}' |
        '\u{E0000}'..='\u{E001F}' | '\u{E0020}'..='\u{E007F}' |
        '\u{E0100}'..='\u{E01EF}')
}

fn load_prefix(path: &Path) -> Result<String, SettingsError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DEFAULT_PREFIX.to_owned());
        }
        Err(_) => return Err(SettingsError::Read),
    };
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| SettingsError::Read)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(SettingsError::FileTooLarge);
    }
    let settings: SettingsFile =
        serde_json::from_slice(&bytes).map_err(|_| SettingsError::MalformedFile)?;
    if settings.version != VERSION {
        return Err(SettingsError::UnsupportedVersion);
    }
    validate_prefix(&settings.prefix)?;
    Ok(settings.prefix)
}

fn write_settings(
    path: &Path,
    bytes: &[u8],
    counter: u64,
) -> Result<Option<std::io::Error>, SettingsError> {
    let parent = path
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or(SettingsError::CreateDirectory)?;
    let temporary_path = parent.join(format!(
        ".settings.json.{}.{}.tmp",
        std::process::id(),
        counter
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary_path)
        .map_err(|_| SettingsError::CreateTemporary)?;
    if file.write_all(bytes).is_err() || file.flush().is_err() {
        drop(file);
        return Err(cleanup(&temporary_path, SettingsError::WriteTemporary));
    }
    if file.sync_all().is_err() {
        drop(file);
        return Err(cleanup(&temporary_path, SettingsError::SyncTemporary));
    }
    drop(file);
    if fs::rename(&temporary_path, path).is_err() {
        return Err(cleanup(&temporary_path, SettingsError::Replace));
    }
    Ok(sync_directory(parent).err())
}

fn cleanup(path: &Path, error: SettingsError) -> SettingsError {
    if let Err(cleanup_error) = fs::remove_file(path) {
        tracing::warn!(
            event = "settings_temporary_cleanup_failed",
            error = %cleanup_error,
            "Settings temporary cleanup failed"
        );
    }
    error
}
fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        OpenOptions::new().read(true).open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    fn path() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "lavis-settings-{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            seq
        ));
        fs::create_dir_all(&d).unwrap();
        d.join("settings.json")
    }
    #[tokio::test]
    async fn defaults_persists_and_rejects_bad_files_without_replacing_them() {
        let path = path();
        let mut store = SettingsStore::load(path.clone()).await.unwrap();
        assert_eq!(store.prefix(), ",");
        store.set_prefix(".".to_owned()).await.unwrap();
        assert_eq!(
            SettingsStore::load(path.clone()).await.unwrap().prefix(),
            "."
        );
        let corrupt = b"not json";
        fs::write(&path, corrupt).unwrap();
        assert!(matches!(
            SettingsStore::load(path.clone()).await,
            Err(SettingsError::MalformedFile)
        ));
        assert_eq!(fs::read(&path).unwrap(), corrupt);
        let unknown = br#"{"version":1,"prefix":".","unexpected":true}"#;
        fs::write(&path, unknown).unwrap();
        assert!(matches!(
            SettingsStore::load(path.clone()).await,
            Err(SettingsError::MalformedFile)
        ));
        assert_eq!(fs::read(&path).unwrap(), unknown);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn rejects_invalid_settings_bytes_without_replacing_them() {
        let path = path();
        let fixtures = [
            (vec![0xff, 0xfe], SettingsError::MalformedFile),
            (
                br#"{"version":2,"prefix":"."}"#.to_vec(),
                SettingsError::UnsupportedVersion,
            ),
            (vec![b'x'; MAX_FILE_BYTES + 1], SettingsError::FileTooLarge),
            (
                br#"{"version":1,"prefix":"\u200b"}"#.to_vec(),
                SettingsError::InvalidPrefix,
            ),
        ];

        for (bytes, expected) in fixtures {
            fs::write(&path, &bytes).unwrap();
            assert!(matches!(
                SettingsStore::load(path.clone()).await,
                Err(error) if error == expected
            ));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn invalid_and_failed_writes_preserve_the_current_prefix_and_file() {
        let path = path();
        let mut store = SettingsStore::load(path.clone()).await.unwrap();
        store.set_prefix(".".to_owned()).await.unwrap();
        let saved = fs::read(&path).unwrap();

        assert_eq!(
            store.set_prefix("invalid".to_owned()).await,
            Err(SettingsError::InvalidPrefix)
        );
        assert_eq!(store.prefix(), ".");
        assert_eq!(fs::read(&path).unwrap(), saved);

        let blocked_temporary = path
            .parent()
            .unwrap()
            .join(format!(".settings.json.{}.2.tmp", std::process::id()));
        fs::create_dir(&blocked_temporary).unwrap();
        assert_eq!(
            store.set_prefix("!".to_owned()).await,
            Err(SettingsError::CreateTemporary)
        );
        assert_eq!(store.prefix(), ".");
        assert_eq!(fs::read(&path).unwrap(), saved);
        fs::remove_dir(&blocked_temporary).unwrap();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persists_settings_files_with_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = path();
        let mut store = SettingsStore::load(path.clone()).await.unwrap();
        store.set_prefix("!".to_owned()).await.unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn validates_unicode_scalars_and_invisible_controls() {
        for prefix in [
            "",
            "abcde",
            "a",
            "Ω",
            " ",
            "\u{00ad}",
            "\u{034f}",
            "\u{061c}",
            "\u{180e}",
            "\u{200b}",
            "\u{202e}",
            "\u{2066}",
            "\u{fe0f}",
            "\u{feff}",
            "\u{e0001}",
            "⚙️",
        ] {
            assert!(validate_prefix(prefix).is_err());
        }
        assert!(validate_prefix("🦀!?_").is_ok());
        assert!(validate_prefix("\u{10ffff}").is_ok());
    }
}
