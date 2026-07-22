use std::{ffi::OsString, fmt, path::PathBuf};

use crate::{credentials::Credentials, error::ConfigError};

pub const API_ID_ENV: &str = "LAVIS_API_ID";
pub const API_HASH_ENV: &str = "LAVIS_API_HASH";

pub struct Config {
    pub api_id: u32,
    api_hash: String,
    pub default_prefix: String,
    pub session_path: PathBuf,
    pub state_dir: PathBuf,
    pub aliases_path: PathBuf,
    pub settings_path: PathBuf,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("api_id", &self.api_id)
            .field("api_hash", &"[REDACTED]")
            .field("default_prefix", &self.default_prefix)
            .field("session_path", &self.session_path)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub session_path: PathBuf,
    pub config_dir: PathBuf,
}

impl ConfigPaths {
    pub fn new(session_path: impl Into<PathBuf>) -> Self {
        Self {
            session_path: session_path.into(),
            config_dir: PathBuf::from("/tmp/lavis-config"),
        }
    }

    pub fn default_with<F>(environment: &F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<OsString>,
    {
        Ok(Self {
            session_path: Self::state_session_path_with(environment)?,
            config_dir: Self::config_dir_with(environment)?,
        })
    }

    pub fn state_session_path_with<F>(environment: &F) -> Result<PathBuf, ConfigError>
    where
        F: Fn(&str) -> Option<OsString>,
    {
        let state_directory = environment("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .or_else(|| {
                environment("HOME")
                    .filter(|home| !home.is_empty())
                    .map(|home| PathBuf::from(home).join(".local/state"))
            })
            .ok_or(ConfigError::MissingStateDirectory)?;
        valid_root(&state_directory)?;
        Ok(state_directory.join("lavis/session"))
    }

    pub fn config_dir_with<F>(environment: &F) -> Result<PathBuf, ConfigError>
    where
        F: Fn(&str) -> Option<OsString>,
    {
        let config_directory = environment("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .or_else(|| {
                environment("HOME")
                    .filter(|home| !home.is_empty())
                    .map(|home| PathBuf::from(home).join(".config"))
            })
            .ok_or(ConfigError::MissingConfigDirectory)?;
        valid_root(&config_directory)?;
        Ok(config_directory.join("lavis"))
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
        Self::from_credentials(Credentials::new(api_id, api_hash)?, paths)
    }

    pub fn from_credentials(
        credentials: Credentials,
        paths: ConfigPaths,
    ) -> Result<Self, ConfigError> {
        if paths.session_path.as_os_str().is_empty() {
            return Err(ConfigError::EmptySessionPath);
        }

        let state_dir = paths
            .session_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .map(PathBuf::from)
            .ok_or(ConfigError::MissingSessionDirectory)?;

        Ok(Self {
            api_id: credentials.api_id(),
            api_hash: credentials.api_hash().to_owned(),
            default_prefix: crate::settings::DEFAULT_PREFIX.to_owned(),
            session_path: paths.session_path,
            aliases_path: state_dir.join("aliases.json"),
            settings_path: state_dir.join("settings.json"),
            state_dir,
        })
    }

    pub fn api_hash(&self) -> &str {
        &self.api_hash
    }
}

pub(crate) fn validate_api_id(api_id: u32) -> Result<u32, ConfigError> {
    if api_id == 0 || i32::try_from(api_id).is_err() {
        return Err(ConfigError::InvalidApiId);
    }
    Ok(api_id)
}

pub(crate) fn validate_api_hash(value: String) -> Result<String, ConfigError> {
    if value.is_empty() {
        return Err(ConfigError::InvalidApiHash);
    }
    Ok(value)
}

fn required_api_id(value: Option<OsString>) -> Result<u32, ConfigError> {
    let value = value.ok_or(ConfigError::MissingApiId)?;
    let value = value.to_str().ok_or(ConfigError::InvalidApiId)?;
    let api_id = value.parse().map_err(|_| ConfigError::InvalidApiId)?;

    validate_api_id(api_id)
}

fn required_api_hash(value: Option<OsString>) -> Result<String, ConfigError> {
    let value = value.ok_or(ConfigError::MissingApiHash)?;
    let value = value
        .into_string()
        .map_err(|_| ConfigError::InvalidApiHash)?;

    validate_api_hash(value)
}

fn valid_root(path: &std::path::Path) -> Result<(), ConfigError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(ConfigError::InvalidDirectory);
    }
    Ok(())
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
        let paths = ConfigPaths::new("/tmp/lavis-test.session");

        let config = Config::load_with(&environment, paths).unwrap();

        assert_eq!(config.api_id, 12345);
        assert_eq!(config.default_prefix, ",");
        assert_eq!(
            config.session_path,
            PathBuf::from("/tmp/lavis-test.session")
        );
        assert_eq!(config.state_dir, PathBuf::from("/tmp"));
        assert_eq!(config.aliases_path, PathBuf::from("/tmp/aliases.json"));
        assert_eq!(config.settings_path, PathBuf::from("/tmp/settings.json"));
        assert_eq!(config.api_hash, "test-api-hash");
        assert!(!format!("{config:?}").contains("test-api-hash"));
    }

    #[test]
    fn distinguishes_missing_and_invalid_api_id() {
        let missing = environment(&[(API_HASH_ENV, "hash")]);
        let invalid = environment(&[(API_ID_ENV, "not-a-number"), (API_HASH_ENV, "hash")]);
        let paths = || ConfigPaths::new("/tmp/session");

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
        let paths = || ConfigPaths::new("/tmp/session");

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
        let xdg = environment(&[
            ("XDG_STATE_HOME", "/tmp/state"),
            ("XDG_CONFIG_HOME", "/tmp/config"),
        ]);
        let home = environment(&[("HOME", "/tmp/home")]);

        assert_eq!(
            ConfigPaths::default_with(&xdg).unwrap().session_path,
            PathBuf::from("/tmp/state/lavis/session")
        );
        let config = Config::load_with(
            &environment(&[(API_ID_ENV, "1"), (API_HASH_ENV, "hash")]),
            ConfigPaths::default_with(&xdg).unwrap(),
        )
        .unwrap();
        assert_eq!(config.state_dir, PathBuf::from("/tmp/state/lavis"));
        assert_eq!(
            config.aliases_path,
            PathBuf::from("/tmp/state/lavis/aliases.json")
        );
        assert_eq!(
            config.settings_path,
            PathBuf::from("/tmp/state/lavis/settings.json")
        );
        assert_eq!(
            ConfigPaths::default_with(&home).unwrap().session_path,
            PathBuf::from("/tmp/home/.local/state/lavis/session")
        );
        assert_eq!(
            ConfigPaths::config_dir_with(&home).unwrap(),
            PathBuf::from("/tmp/home/.config/lavis")
        );
    }

    #[test]
    fn empty_xdg_roots_fall_back_to_home_and_relative_roots_are_rejected() {
        assert_eq!(
            ConfigPaths::default_with(&environment(&[
                ("XDG_STATE_HOME", "state"),
                ("XDG_CONFIG_HOME", "/tmp/config"),
            ])),
            Err(ConfigError::InvalidDirectory)
        );
        assert_eq!(
            ConfigPaths::default_with(&environment(&[
                ("XDG_STATE_HOME", ""),
                ("XDG_CONFIG_HOME", ""),
                ("HOME", "/tmp/home"),
            ]))
            .unwrap()
            .config_dir,
            PathBuf::from("/tmp/home/.config/lavis")
        );
    }
}
