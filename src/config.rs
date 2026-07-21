use std::{ffi::OsString, fmt, path::PathBuf};

use crate::error::ConfigError;

pub const API_ID_ENV: &str = "LAVIS_API_ID";
pub const API_HASH_ENV: &str = "LAVIS_API_HASH";

pub struct Config {
    pub api_id: u32,
    api_hash: String,
    pub prefix: String,
    pub session_path: PathBuf,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("api_id", &self.api_id)
            .field("api_hash", &"[REDACTED]")
            .field("prefix", &self.prefix)
            .field("session_path", &self.session_path)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub prefix: String,
    pub session_path: PathBuf,
}

impl ConfigPaths {
    pub fn new(prefix: impl Into<String>, session_path: impl Into<PathBuf>) -> Self {
        Self {
            prefix: prefix.into(),
            session_path: session_path.into(),
        }
    }

    pub fn default_with<F>(environment: &F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<OsString>,
    {
        let state_directory = environment("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| environment("HOME").map(|home| PathBuf::from(home).join(".local/state")))
            .ok_or(ConfigError::MissingStateDirectory)?;

        Ok(Self::new(",", state_directory.join("lavis/session")))
    }
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        let environment = |name: &str| std::env::var_os(name);
        let paths = ConfigPaths::default_with(&environment)?;
        Self::load_with(&environment, paths)
    }

    pub fn load_with<F>(environment: &F, paths: ConfigPaths) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<OsString>,
    {
        let api_id = required_api_id(environment(API_ID_ENV))?;
        let api_hash = required_api_hash(environment(API_HASH_ENV))?;

        if paths.prefix.is_empty() {
            return Err(ConfigError::EmptyPrefix);
        }
        if paths.session_path.as_os_str().is_empty() {
            return Err(ConfigError::EmptySessionPath);
        }

        Ok(Self {
            api_id,
            api_hash,
            prefix: paths.prefix,
            session_path: paths.session_path,
        })
    }

    pub fn api_hash(&self) -> &str {
        &self.api_hash
    }
}

fn required_api_id(value: Option<OsString>) -> Result<u32, ConfigError> {
    let value = value.ok_or(ConfigError::MissingApiId)?;
    let value = value.to_str().ok_or(ConfigError::InvalidApiId)?;
    let api_id = value.parse().map_err(|_| ConfigError::InvalidApiId)?;

    if api_id == 0 || i32::try_from(api_id).is_err() {
        return Err(ConfigError::InvalidApiId);
    }

    Ok(api_id)
}

fn required_api_hash(value: Option<OsString>) -> Result<String, ConfigError> {
    let value = value.ok_or(ConfigError::MissingApiHash)?;
    let value = value
        .into_string()
        .map_err(|_| ConfigError::InvalidApiHash)?;

    if value.is_empty() {
        return Err(ConfigError::InvalidApiHash);
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::OsString, path::PathBuf};

    use super::{API_HASH_ENV, API_ID_ENV, Config, ConfigPaths};
    use crate::error::ConfigError;

    fn environment(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(*value)))
            .collect::<HashMap<_, _>>();
        move |name| values.get(name).cloned()
    }

    #[test]
    fn loads_configuration_from_injected_environment_and_paths() {
        let environment = environment(&[(API_ID_ENV, "12345"), (API_HASH_ENV, "test-api-hash")]);
        let paths = ConfigPaths::new("!", "/tmp/lavis-test.session");

        let config = Config::load_with(&environment, paths).unwrap();

        assert_eq!(config.api_id, 12345);
        assert_eq!(config.prefix, "!");
        assert_eq!(
            config.session_path,
            PathBuf::from("/tmp/lavis-test.session")
        );
        assert_eq!(config.api_hash, "test-api-hash");
        assert!(!format!("{config:?}").contains("test-api-hash"));
    }

    #[test]
    fn distinguishes_missing_and_invalid_api_id() {
        let missing = environment(&[(API_HASH_ENV, "hash")]);
        let invalid = environment(&[(API_ID_ENV, "not-a-number"), (API_HASH_ENV, "hash")]);
        let paths = || ConfigPaths::new(".", "/tmp/session");

        assert!(matches!(
            Config::load_with(&missing, paths()),
            Err(ConfigError::MissingApiId)
        ));
        assert!(matches!(
            Config::load_with(&invalid, paths()),
            Err(ConfigError::InvalidApiId)
        ));
        let out_of_range = environment(&[(API_ID_ENV, "2147483648"), (API_HASH_ENV, "hash")]);
        assert!(matches!(
            Config::load_with(&out_of_range, paths()),
            Err(ConfigError::InvalidApiId)
        ));
    }

    #[test]
    fn rejects_missing_or_empty_api_hash_without_exposing_it() {
        let missing = environment(&[(API_ID_ENV, "1")]);
        let empty = environment(&[(API_ID_ENV, "1"), (API_HASH_ENV, "")]);
        let paths = || ConfigPaths::new(".", "/tmp/session");

        assert!(matches!(
            Config::load_with(&missing, paths()),
            Err(ConfigError::MissingApiHash)
        ));
        assert!(matches!(
            Config::load_with(&empty, paths()),
            Err(ConfigError::InvalidApiHash)
        ));
    }

    #[test]
    fn derives_xdg_compatible_session_paths_from_injected_environment() {
        let xdg = environment(&[("XDG_STATE_HOME", "/tmp/state")]);
        let home = environment(&[("HOME", "/tmp/home")]);

        assert_eq!(
            ConfigPaths::default_with(&xdg).unwrap().session_path,
            PathBuf::from("/tmp/state/lavis/session")
        );
        assert_eq!(ConfigPaths::default_with(&xdg).unwrap().prefix, ",");
        assert_eq!(
            ConfigPaths::default_with(&home).unwrap().session_path,
            PathBuf::from("/tmp/home/.local/state/lavis/session")
        );
    }
}
