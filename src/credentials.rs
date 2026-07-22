use std::{
    ffi::OsString,
    fmt,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    config::{API_HASH_ENV, API_ID_ENV, validate_api_hash, validate_api_id},
    error::{ConfigError, CredentialsError},
};

const VERSION: u32 = 1;
const MAX_FILE_BYTES: usize = 4096;
pub const ONBOARDING_DISCLAIMER: &str = concat!(
    "Store these credentials securely. Do not commit credentials.json, your Telegram session, ",
    "or your API hash. This does not make the account or userbot legal, safe, or approved by ",
    "Telegram.",
);

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    api_id: u32,
    api_hash: String,
}

impl Credentials {
    pub fn new(api_id: u32, api_hash: String) -> Result<Self, ConfigError> {
        Ok(Self {
            api_id: validate_api_id(api_id)?,
            api_hash: validate_api_hash(api_hash)?,
        })
    }

    pub fn api_id(&self) -> u32 {
        self.api_id
    }

    pub fn api_hash(&self) -> &str {
        &self.api_hash
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("api_id", &self.api_id)
            .field("api_hash", &"[REDACTED]")
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialsFile {
    version: u32,
    api_id: u32,
    api_hash: String,
}

pub struct CredentialsStore {
    path: PathBuf,
    credentials: Option<Credentials>,
    temporary_counter: u64,
}

impl CredentialsStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            credentials: None,
            temporary_counter: 0,
        }
    }

    pub fn credentials(&self) -> Option<&Credentials> {
        self.credentials.as_ref()
    }

    pub fn load(path: PathBuf) -> Result<Self, CredentialsError> {
        let credentials = read_credentials(&path)?;
        Ok(Self {
            path,
            credentials: Some(credentials),
            temporary_counter: 0,
        })
    }

    pub fn save(&mut self, credentials: Credentials) -> Result<(), CredentialsError> {
        let bytes = serialize_credentials(&credentials)?;
        let temporary_counter = self.temporary_counter.saturating_add(1);
        write_credentials(&self.path, &bytes, temporary_counter)?;
        self.temporary_counter = temporary_counter;
        self.credentials = Some(credentials);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    Environment,
    Stored,
}

pub fn resolve_environment<F>(environment: &F) -> Result<Option<Credentials>, CredentialsError>
where
    F: Fn(&str) -> Option<OsString>,
{
    match (environment(API_ID_ENV), environment(API_HASH_ENV)) {
        (Some(id), Some(hash)) => credentials_from_env(id, hash).map(Some),
        (None, None) => Ok(None),
        _ => Err(CredentialsError::PartialEnvironment),
    }
}

pub fn resolve_stored(path: PathBuf) -> Result<(Credentials, CredentialSource), CredentialsError> {
    let store = CredentialsStore::load(path)?;
    let credentials = store.credentials.ok_or(CredentialsError::Read)?;
    Ok((credentials, CredentialSource::Stored))
}

pub fn resolve<F>(
    environment: &F,
    path: PathBuf,
) -> Result<(Credentials, CredentialSource), CredentialsError>
where
    F: Fn(&str) -> Option<OsString>,
{
    match resolve_environment(environment)? {
        Some(credentials) => Ok((credentials, CredentialSource::Environment)),
        None => resolve_stored(path),
    }
}

pub fn credentials_path(config_dir: PathBuf) -> PathBuf {
    config_dir.join("credentials.json")
}

pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub fn onboard(path: PathBuf) -> Result<Credentials, CredentialsError> {
    println!("Telegram API credentials are available from https://my.telegram.org.");
    let api_id = prompt("Telegram API ID: ")?
        .parse()
        .map_err(|_| CredentialsError::InvalidApiId)?;
    let api_hash =
        rpassword::prompt_password("Telegram API hash: ").map_err(|_| CredentialsError::Read)?;
    let credentials = map_config_error(Credentials::new(api_id, api_hash))?;

    println!("{ONBOARDING_DISCLAIMER}");
    require_confirmation(&prompt("Save these credentials and continue? [y/N] ")?)?;

    let mut store = CredentialsStore::new(path);
    store.save(credentials.clone())?;
    Ok(credentials)
}

pub fn confirmed(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn require_confirmation(value: &str) -> Result<(), CredentialsError> {
    if confirmed(value) {
        Ok(())
    } else {
        Err(CredentialsError::Cancelled)
    }
}

fn credentials_from_env(id: OsString, hash: OsString) -> Result<Credentials, CredentialsError> {
    let api_id = id
        .to_str()
        .ok_or(CredentialsError::InvalidApiId)?
        .parse()
        .map_err(|_| CredentialsError::InvalidApiId)?;
    let api_hash = hash
        .into_string()
        .map_err(|_| CredentialsError::InvalidApiHash)?;
    map_config_error(Credentials::new(api_id, api_hash))
}

fn map_config_error(
    result: Result<Credentials, ConfigError>,
) -> Result<Credentials, CredentialsError> {
    result.map_err(|error| match error {
        ConfigError::InvalidApiId => CredentialsError::InvalidApiId,
        _ => CredentialsError::InvalidApiHash,
    })
}

fn prompt(message: &str) -> Result<String, CredentialsError> {
    print!("{message}");
    io::stdout().flush().map_err(|_| CredentialsError::Read)?;
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .map_err(|_| CredentialsError::Read)?;
    Ok(value.trim().to_owned())
}

fn serialize_credentials(credentials: &Credentials) -> Result<Vec<u8>, CredentialsError> {
    let bytes = serde_json::to_vec_pretty(&CredentialsFile {
        version: VERSION,
        api_id: credentials.api_id,
        api_hash: credentials.api_hash.clone(),
    })
    .map_err(|_| CredentialsError::WriteTemporary)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(CredentialsError::FileTooLarge);
    }
    Ok(bytes)
}

fn read_credentials(path: &Path) -> Result<Credentials, CredentialsError> {
    if let Some(parent) = path.parent() {
        validate_existing_directory(parent)?;
    }
    validate_existing_file(path)?;
    let file = fs::File::open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            CredentialsError::NotFound
        } else {
            CredentialsError::Read
        }
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| CredentialsError::Read)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(CredentialsError::FileTooLarge);
    }
    let file: CredentialsFile =
        serde_json::from_slice(&bytes).map_err(|_| CredentialsError::MalformedFile)?;
    if file.version != VERSION {
        return Err(CredentialsError::UnsupportedVersion);
    }
    map_config_error(Credentials::new(file.api_id, file.api_hash))
}

fn write_credentials(path: &Path, bytes: &[u8], counter: u64) -> Result<(), CredentialsError> {
    write_credentials_with(path, bytes, counter, fs::rename)
}

fn write_credentials_with<F>(
    path: &Path,
    bytes: &[u8],
    counter: u64,
    replace: F,
) -> Result<(), CredentialsError>
where
    F: FnOnce(&Path, &Path) -> io::Result<()>,
{
    let parent = path.parent().ok_or(CredentialsError::CreateDirectory)?;
    ensure_secure_directory(parent)?;
    validate_existing_file(path)?;
    let temporary = parent.join(format!(
        ".credentials.json.{}.{}.tmp",
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
        .map_err(|_| CredentialsError::CreateTemporary)?;
    let mode_result = file
        .metadata()
        .map_err(|_| CredentialsError::CreateTemporary)
        .and_then(|metadata| validate_mode(&metadata, 0o600));
    if let Err(error) = mode_result {
        drop(file);
        return Err(cleanup_temporary(&temporary, error));
    }
    if file.write_all(bytes).is_err() || file.flush().is_err() {
        drop(file);
        return Err(cleanup_temporary(
            &temporary,
            CredentialsError::WriteTemporary,
        ));
    }
    if file.sync_all().is_err() {
        drop(file);
        return Err(cleanup_temporary(
            &temporary,
            CredentialsError::SyncTemporary,
        ));
    }
    drop(file);
    if replace(&temporary, path).is_err() {
        return Err(cleanup_temporary(&temporary, CredentialsError::Replace));
    }
    sync_directory(parent).map_err(|_| CredentialsError::SyncDirectory)
}

fn cleanup_temporary(path: &Path, error: CredentialsError) -> CredentialsError {
    if let Err(cleanup_error) = fs::remove_file(path) {
        tracing::warn!(
            event = "credentials_temporary_cleanup_failed",
            error = %cleanup_error,
            "Credentials temporary cleanup failed"
        );
    }
    error
}

fn ensure_secure_directory(path: &Path) -> Result<(), CredentialsError> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_existing_directory(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|_| CredentialsError::CreateDirectory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                    .map_err(|_| CredentialsError::CreateDirectory)?;
            }
            validate_existing_directory(path)
        }
        Err(_) => Err(CredentialsError::CreateDirectory),
    }
}

fn validate_existing_directory(path: &Path) -> Result<(), CredentialsError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            validate_mode(&metadata, 0o700)
        }
        Ok(_) => Err(CredentialsError::UnsafeStorage),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(CredentialsError::Read),
    }
}

fn validate_existing_file(path: &Path) -> Result<(), CredentialsError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            validate_mode(&metadata, 0o600)
        }
        Ok(_) => Err(CredentialsError::UnsafeStorage),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(CredentialsError::Read),
    }
}

fn validate_mode(metadata: &fs::Metadata, expected: u32) -> Result<(), CredentialsError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != expected {
            return Err(CredentialsError::UnsafeStorage);
        }
    }
    #[cfg(not(unix))]
    let _ = (metadata, expected);
    Ok(())
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
    use std::{
        collections::HashMap,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn path() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "lavis-credentials-{}",
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
        directory.join("credentials.json")
    }

    fn env(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(*value)))
            .collect::<HashMap<_, _>>();
        move |key| values.get(key).cloned()
    }

    fn credentials() -> Credentials {
        Credentials::new(1, "test-hash-not-real".to_owned()).unwrap()
    }

    #[test]
    fn disclaimer_is_exact() {
        assert_eq!(
            ONBOARDING_DISCLAIMER,
            "Store these credentials securely. Do not commit credentials.json, your Telegram session, or your API hash. This does not make the account or userbot legal, safe, or approved by Telegram."
        );
    }

    #[test]
    fn environment_precedence_invalid_values_and_partial_rejection() {
        assert_eq!(
            resolve_environment(&env(&[
                (API_ID_ENV, "2"),
                (API_HASH_ENV, "env-hash-not-real"),
            ]))
            .unwrap()
            .unwrap()
            .api_id(),
            2
        );
        assert_eq!(
            resolve_environment(&env(&[(API_ID_ENV, "not-an-id"), (API_HASH_ENV, "x")]))
                .unwrap_err(),
            CredentialsError::InvalidApiId
        );
        assert_eq!(
            resolve_environment(&env(&[(API_ID_ENV, "2")])),
            Err(CredentialsError::PartialEnvironment)
        );
    }

    #[test]
    fn stored_credentials_are_used_only_when_both_environment_values_are_missing() {
        let path = path();
        let mut store = CredentialsStore::new(path.clone());
        store.save(credentials()).unwrap();
        assert_eq!(
            resolve(&env(&[]), path.clone()).unwrap().1,
            CredentialSource::Stored
        );
        assert_eq!(
            resolve(&env(&[(API_ID_ENV, "invalid")]), path).unwrap_err(),
            CredentialsError::PartialEnvironment
        );
    }

    #[test]
    fn invalid_complete_environment_does_not_fall_back_to_stored_credentials() {
        let path = path();
        let mut store = CredentialsStore::new(path.clone());
        store.save(credentials()).unwrap();
        assert_eq!(
            resolve(
                &env(&[(API_ID_ENV, "invalid"), (API_HASH_ENV, "not-a-secret")]),
                path,
            )
            .unwrap_err(),
            CredentialsError::InvalidApiId
        );
    }

    #[test]
    fn round_trip_unknown_oversized_and_unsupported_files_remain_untouched() {
        let path = path();
        let mut store = CredentialsStore::new(path.clone());
        store.save(credentials()).unwrap();
        let loaded = CredentialsStore::load(path.clone()).unwrap();
        assert_eq!(loaded.credentials(), Some(&credentials()));
        let oversized = vec![b'x'; MAX_FILE_BYTES + 1];
        for bytes in [
            b"bad".as_slice(),
            br#"{"version":1,"api_id":1,"api_hash":"x","unknown":true}"#.as_slice(),
            br#"{"version":2,"api_id":1,"api_hash":"x"}"#.as_slice(),
            oversized.as_slice(),
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(CredentialsStore::load(path.clone()).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn oversized_save_preserves_old_storage_and_memory() {
        let path = path();
        let mut store = CredentialsStore::new(path.clone());
        store.save(credentials()).unwrap();
        let old_bytes = fs::read(&path).unwrap();
        let oversized = Credentials::new(1, "x".repeat(MAX_FILE_BYTES)).unwrap();
        assert_eq!(store.save(oversized), Err(CredentialsError::FileTooLarge));
        assert_eq!(store.credentials(), Some(&credentials()));
        assert_eq!(fs::read(path).unwrap(), old_bytes);
    }

    #[test]
    fn confirmation_defaults_to_no_and_cancellation_preserves_storage() {
        assert!(!confirmed(""));
        assert!(!confirmed("no"));
        assert!(confirmed("yes"));
        assert_eq!(require_confirmation(""), Err(CredentialsError::Cancelled));
        let missing_store = CredentialsStore::new(path());
        assert!(missing_store.credentials().is_none());

        let path = path();
        let mut store = CredentialsStore::new(path.clone());
        store.save(credentials()).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(require_confirmation("no"), Err(CredentialsError::Cancelled));
        assert_eq!(store.credentials(), Some(&credentials()));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn debug_and_errors_do_not_expose_hash() {
        let credentials = credentials();
        assert!(!format!("{credentials:?}").contains(credentials.api_hash()));
        assert!(
            !CredentialsError::InvalidApiHash
                .to_string()
                .contains(credentials.api_hash())
        );
    }

    #[test]
    fn failed_persistence_preserves_old_storage_and_memory() {
        let path = path();
        let mut store = CredentialsStore::new(path.clone());
        store.save(credentials()).unwrap();
        let old_bytes = fs::read(&path).unwrap();
        let temporary = path.parent().unwrap().join(format!(
            ".credentials.json.{}.2.tmp",
            std::process::id(),
        ));
        fs::create_dir(&temporary).unwrap();
        assert_eq!(
            store.save(Credentials::new(2, "other-test-hash".to_owned()).unwrap()),
            Err(CredentialsError::CreateTemporary)
        );
        assert_eq!(store.credentials(), Some(&credentials()));
        assert_eq!(fs::read(path).unwrap(), old_bytes);
    }

    #[test]
    fn failed_replacement_preserves_old_storage_and_in_memory_credentials() {
        let path = path();
        let mut store = CredentialsStore::new(path.clone());
        store.save(credentials()).unwrap();
        let old_bytes = fs::read(&path).unwrap();
        let replacement = Credentials::new(2, "other-test-hash".to_owned()).unwrap();
        let bytes = serialize_credentials(&replacement).unwrap();

        assert_eq!(
            write_credentials_with(&path, &bytes, 2, |_, _| {
                Err(io::Error::other("replacement failed"))
            }),
            Err(CredentialsError::Replace)
        );
        let temporary = path.parent().unwrap().join(format!(
            ".credentials.json.{}.2.tmp",
            std::process::id()
        ));
        assert!(!temporary.exists());
        assert_eq!(store.credentials(), Some(&credentials()));
        assert_eq!(fs::read(path).unwrap(), old_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn strict_permissions_and_unsafe_types_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let path = path();
        let mut store = CredentialsStore::new(path.clone());
        store.save(credentials()).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            CredentialsStore::load(path.clone()),
            Err(CredentialsError::UnsafeStorage)
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o750)).unwrap();
        assert!(matches!(
            CredentialsStore::load(path.clone()),
            Err(CredentialsError::UnsafeStorage)
        ));
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            CredentialsStore::load(path.clone()),
            Err(CredentialsError::UnsafeStorage)
        ));
        fs::remove_dir(&path).unwrap();
        symlink("elsewhere", &path).unwrap();
        assert!(matches!(
            CredentialsStore::load(path.clone()),
            Err(CredentialsError::UnsafeStorage)
        ));
        let directory = path.parent().unwrap().to_path_buf();
        let target = directory.with_extension("target");
        fs::create_dir(&target).unwrap();
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&directory).unwrap();
        symlink(&target, &directory).unwrap();
        assert!(matches!(
            CredentialsStore::load(path),
            Err(CredentialsError::UnsafeStorage)
        ));
    }
}
