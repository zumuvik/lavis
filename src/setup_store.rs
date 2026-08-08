//! Durable, local-only storage for companion setup progress and its token.
//!
//! The state document intentionally cannot represent a token. The token is kept
//! in a separate, permission-checked file so routine setup state inspection
//! cannot disclose a credential.

use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::error::SetupStoreError;

pub const SETUP_STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: usize = 16 * 1024;
const MAX_TOKEN_BYTES: usize = 512;

#[derive(Clone, Eq, PartialEq)]
pub struct CompanionToken(String);

impl CompanionToken {
    pub fn new(value: String) -> Result<Self, SetupStoreError> {
        if value.is_empty() || value.len() > MAX_TOKEN_BYTES || value.contains(['\n', '\r', '\0']) {
            return Err(SetupStoreError::InvalidToken);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CompanionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CompanionToken([REDACTED])")
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupStages {
    pub bot_created: bool,
    /// A Bot API identity was verified and durably recorded before writing the
    /// separate token file. This prevents a token-write crash from allowing a
    /// second BotFather creation attempt.
    #[serde(default)]
    pub bot_identity_recorded: bool,
    #[serde(default)]
    pub bot_dialog_initialized: bool,
    #[serde(default)]
    pub app_config_checked: bool,
    #[serde(default)]
    pub forum_group_created: bool,
    #[serde(default)]
    pub forum_topic_created: bool,
    #[serde(default)]
    pub bot_invited: bool,
    #[serde(default)]
    pub bot_rights_configured: bool,
    #[serde(default)]
    pub folder_configured: bool,
    #[serde(default)]
    pub community_joined: bool,
    pub companion_configured: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupIdentities {
    pub bot_username: Option<String>,
    #[serde(default)]
    pub bot_user_id: Option<i64>,
    #[serde(default)]
    pub bot_access_hash: Option<i64>,
    pub owner_user_id: Option<i64>,
    pub companion_chat_id: Option<i64>,
    #[serde(default)]
    pub companion_chat_access_hash: Option<i64>,
    #[serde(default)]
    pub companion_topic_id: Option<i32>,
    #[serde(default)]
    pub companion_logs_topic_id: Option<i32>,
    #[serde(default)]
    pub companion_backups_topic_id: Option<i32>,
    #[serde(default)]
    pub companion_folder_id: Option<i32>,
    #[serde(default)]
    pub community_chat_id: Option<i64>,
    #[serde(default)]
    pub community_access_hash: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedSetupState {
    version: u32,
    pub model: String,
    pub stages: SetupStages,
    pub status: String,
    pub identities: SetupIdentities,
}

impl Default for PersistedSetupState {
    fn default() -> Self {
        Self {
            version: SETUP_STATE_VERSION,
            model: "companion".to_owned(),
            stages: SetupStages::default(),
            status: "idle".to_owned(),
            identities: SetupIdentities::default(),
        }
    }
}

impl PersistedSetupState {
    pub fn version(&self) -> u32 {
        self.version
    }
}

pub struct SetupStore {
    state_path: PathBuf,
    token_path: PathBuf,
    temporary_counter: u64,
}

impl SetupStore {
    pub fn new(state_path: PathBuf, token_path: PathBuf) -> Self {
        Self {
            state_path,
            token_path,
            temporary_counter: 0,
        }
    }

    pub fn load_state(&self) -> Result<PersistedSetupState, SetupStoreError> {
        let bytes = read_checked(&self.state_path, MAX_STATE_BYTES)?;
        let state: PersistedSetupState =
            serde_json::from_slice(&bytes).map_err(|_| SetupStoreError::MalformedState)?;
        if state.version != SETUP_STATE_VERSION {
            return Err(SetupStoreError::UnsupportedVersion);
        }
        Ok(state)
    }

    pub fn save_state(&mut self, state: &PersistedSetupState) -> Result<(), SetupStoreError> {
        if state.version != SETUP_STATE_VERSION {
            return Err(SetupStoreError::UnsupportedVersion);
        }
        let bytes =
            serde_json::to_vec_pretty(state).map_err(|_| SetupStoreError::WriteTemporary)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(SetupStoreError::FileTooLarge);
        }
        self.write(&self.state_path.clone(), &bytes)
    }

    pub fn load_token(&self) -> Result<CompanionToken, SetupStoreError> {
        let bytes = read_checked(&self.token_path, MAX_TOKEN_BYTES)?;
        let token = std::str::from_utf8(&bytes).map_err(|_| SetupStoreError::MalformedState)?;
        CompanionToken::new(token.strip_suffix('\n').unwrap_or(token).to_owned())
    }

    pub fn save_token(&mut self, token: &CompanionToken) -> Result<(), SetupStoreError> {
        self.write(&self.token_path.clone(), token.as_str().as_bytes())
    }

    fn write(&mut self, path: &Path, bytes: &[u8]) -> Result<(), SetupStoreError> {
        self.temporary_counter = self.temporary_counter.saturating_add(1);
        write_checked(path, bytes, self.temporary_counter)
    }
}

fn read_checked(path: &Path, maximum: usize) -> Result<Vec<u8>, SetupStoreError> {
    let parent = path.parent().ok_or(SetupStoreError::Read)?;
    validate_parent(parent, false)?;
    validate_file(path)?;
    let file = fs::File::open(path).map_err(map_read_error)?;
    let mut bytes = Vec::new();
    file.take((maximum + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| SetupStoreError::Read)?;
    if bytes.len() > maximum {
        return Err(SetupStoreError::FileTooLarge);
    }
    Ok(bytes)
}

fn write_checked(path: &Path, bytes: &[u8], counter: u64) -> Result<(), SetupStoreError> {
    let parent = path.parent().ok_or(SetupStoreError::CreateDirectory)?;
    validate_parent(parent, true)?;
    validate_file_if_present(path)?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("setup"),
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
        .open(&temporary)
        .map_err(|_| SetupStoreError::CreateTemporary)?;
    if file.write_all(bytes).is_err() || file.flush().is_err() {
        drop(file);
        return cleanup(&temporary, SetupStoreError::WriteTemporary);
    }
    if file.sync_all().is_err() {
        drop(file);
        return cleanup(&temporary, SetupStoreError::SyncTemporary);
    }
    drop(file);
    if fs::rename(&temporary, path).is_err() {
        return cleanup(&temporary, SetupStoreError::Replace);
    }
    sync_directory(parent).map_err(|_| SetupStoreError::SyncDirectory)
}

fn cleanup(path: &Path, error: SetupStoreError) -> Result<(), SetupStoreError> {
    let _ = fs::remove_file(path);
    Err(error)
}

fn validate_parent(path: &Path, create: bool) -> Result<(), SetupStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            if create {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                        .map_err(|_| SetupStoreError::CreateDirectory)?;
                }
                let metadata =
                    fs::symlink_metadata(path).map_err(|_| SetupStoreError::CreateDirectory)?;
                validate_mode(&metadata, 0o700)
            } else {
                validate_mode(&metadata, 0o700)
            }
        }
        Ok(_) => Err(SetupStoreError::UnsafeStorage),
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            fs::create_dir_all(path).map_err(|_| SetupStoreError::CreateDirectory)?;
            validate_parent(path, false)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(SetupStoreError::NotFound),
        Err(_) => Err(SetupStoreError::Read),
    }
}

fn validate_file(path: &Path) -> Result<(), SetupStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            validate_mode(&metadata, 0o600)
        }
        Ok(_) => Err(SetupStoreError::UnsafeStorage),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(SetupStoreError::NotFound),
        Err(_) => Err(SetupStoreError::Read),
    }
}

fn validate_file_if_present(path: &Path) -> Result<(), SetupStoreError> {
    match validate_file(path) {
        Err(SetupStoreError::NotFound) => Ok(()),
        result => result,
    }
}

fn validate_mode(metadata: &fs::Metadata, expected: u32) -> Result<(), SetupStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != expected {
            return Err(SetupStoreError::UnsafeStorage);
        }
    }
    #[cfg(not(unix))]
    let _ = (metadata, expected);
    Ok(())
}

fn map_read_error(error: io::Error) -> SetupStoreError {
    if error.kind() == io::ErrorKind::NotFound {
        SetupStoreError::NotFound
    } else {
        SetupStoreError::Read
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn store() -> (SetupStore, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "lavis-setup-{}-{seq}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        (
            SetupStore::new(
                directory.join("setup.json"),
                directory.join("companion-bot.token"),
            ),
            directory,
        )
    }

    #[test]
    fn state_is_versioned_and_never_contains_token() {
        let (mut store, directory) = store();
        let token = CompanionToken::new("123456:abcdefghijklmnopqrstUVWX".to_owned()).unwrap();
        store.save_state(&PersistedSetupState::default()).unwrap();
        store.save_token(&token).unwrap();
        let state = fs::read_to_string(directory.join("setup.json")).unwrap();
        assert!(state.contains("\"version\": 1"));
        assert!(state.contains("\"model\""));
        assert!(state.contains("\"stages\""));
        assert!(state.contains("\"status\""));
        assert!(state.contains("\"identities\""));
        assert!(!state.contains(token.as_str()));
        assert_eq!(store.load_token().unwrap(), token);
        assert!(!format!("{token:?}").contains(token.as_str()));
    }

    #[test]
    fn identity_recorded_stage_round_trips_for_partial_credential_recovery() {
        let (mut store, directory) = store();
        let mut state = PersistedSetupState::default();
        state.stages.bot_identity_recorded = true;
        state.identities.bot_username = Some("lavis_test_bot".to_owned());
        state.identities.bot_user_id = Some(7);
        store.save_state(&state).unwrap();

        let loaded = store.load_state().unwrap();
        assert!(loaded.stages.bot_identity_recorded);
        assert!(!loaded.stages.bot_created);
        assert_eq!(loaded.identities.bot_user_id, Some(7));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn legacy_companion_state_defaults_new_community_fields() {
        let state: PersistedSetupState = serde_json::from_str(
            r#"{
                "version": 1,
                "model": "companion",
                "stages": {
                    "bot_created": true,
                    "companion_configured": true
                },
                "status": "companion_configured",
                "identities": {
                    "bot_username": "lavis_test_bot",
                    "owner_user_id": 1,
                    "companion_chat_id": 42
                }
            }"#,
        )
        .unwrap();

        assert!(!state.stages.community_joined);
        assert_eq!(state.identities.community_chat_id, None);
        assert_eq!(state.identities.community_access_hash, None);
        assert_eq!(state.identities.companion_folder_id, None);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_and_enforces_modes() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (mut store, directory) = store();
        store
            .save_token(&CompanionToken::new("123456:abcdefghijklmnopqrstUVWX".into()).unwrap())
            .unwrap();
        let token_path = directory.join("companion-bot.token");
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&token_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(&token_path).unwrap();
        symlink("elsewhere", &token_path).unwrap();
        assert_eq!(store.load_token(), Err(SetupStoreError::UnsafeStorage));
    }

    #[test]
    fn replacement_failure_leaves_previous_state_intact() {
        let (mut store, directory) = store();
        store.save_state(&PersistedSetupState::default()).unwrap();
        let previous = fs::read(directory.join("setup.json")).unwrap();
        fs::remove_file(directory.join("setup.json")).unwrap();
        fs::create_dir(directory.join("setup.json")).unwrap();
        assert_eq!(
            store.save_state(&PersistedSetupState::default()),
            Err(SetupStoreError::UnsafeStorage)
        );
        fs::remove_dir(directory.join("setup.json")).unwrap();
        fs::write(directory.join("setup.json"), previous).unwrap();
    }

    #[test]
    fn writes_replace_state_without_leaving_a_temporary_file() {
        let (mut store, directory) = store();
        let mut state = PersistedSetupState::default();
        store.save_state(&state).unwrap();
        state.status = "complete".to_owned();
        store.save_state(&state).unwrap();
        assert_eq!(store.load_state().unwrap(), state);
        assert!(
            !directory
                .join(format!(".setup.json.{}.2.tmp", std::process::id()))
                .exists()
        );
    }
}
