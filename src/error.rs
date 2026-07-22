use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("LAVIS_API_ID is not set")]
    MissingApiId,
    #[error("LAVIS_API_ID must be a positive integer")]
    InvalidApiId,
    #[error("LAVIS_API_HASH is not set")]
    MissingApiHash,
    #[error("LAVIS_API_HASH must be a non-empty Unicode value")]
    InvalidApiHash,
    #[error("command prefix must not be empty")]
    EmptyPrefix,
    #[error("session path must not be empty")]
    EmptySessionPath,
    #[error("session path has no parent directory for application state")]
    MissingSessionDirectory,
    #[error("neither XDG_STATE_HOME nor HOME is available for the session path")]
    MissingStateDirectory,
    #[error("neither XDG_CONFIG_HOME nor HOME is available for the credentials path")]
    MissingConfigDirectory,
    #[error("configuration and state directories must be absolute, non-empty paths")]
    InvalidDirectory,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CredentialsError {
    #[error("credential onboarding was cancelled")]
    Cancelled,
    #[error("failed to read credentials input or storage")]
    Read,
    #[error("credentials file was not found")]
    NotFound,
    #[error("credential API ID must be a positive integer")]
    InvalidApiId,
    #[error("credential API hash must be a non-empty Unicode value")]
    InvalidApiHash,
    #[error("LAVIS_API_ID and LAVIS_API_HASH must be set together")]
    PartialEnvironment,
    #[error("credentials file is too large")]
    FileTooLarge,
    #[error("credentials file is malformed")]
    MalformedFile,
    #[error("credentials file version is unsupported")]
    UnsupportedVersion,
    #[error("credentials storage has unsafe permissions or type")]
    UnsafeStorage,
    #[error("failed to create credentials directory")]
    CreateDirectory,
    #[error("failed to create credentials temporary file")]
    CreateTemporary,
    #[error("failed to write credentials temporary file")]
    WriteTemporary,
    #[error("failed to synchronize credentials temporary file")]
    SyncTemporary,
    #[error("failed to synchronize credentials directory")]
    SyncDirectory,
    #[error("failed to replace credentials file")]
    Replace,
    #[error("failed to remove credentials file")]
    Delete,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SettingsError {
    #[error("settings prefix must be 1–4 visible non-alphabetic characters")]
    InvalidPrefix,
    #[error("settings file is too large")]
    FileTooLarge,
    #[error("settings file is malformed")]
    MalformedFile,
    #[error("settings file version is unsupported")]
    UnsupportedVersion,
    #[error("failed to read settings storage")]
    Read,
    #[error("settings state directory is unavailable")]
    CreateDirectory,
    #[error("failed to create settings temporary file")]
    CreateTemporary,
    #[error("failed to write settings temporary file")]
    WriteTemporary,
    #[error("failed to synchronize settings temporary file")]
    SyncTemporary,
    #[error("failed to replace settings file")]
    Replace,
    #[error("failed to run settings storage task")]
    StorageTask,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AliasError {
    #[error("alias name is invalid")]
    InvalidName,
    #[error("alias name conflicts with a built-in command")]
    ReservedName,
    #[error("alias name 'prefix' is reserved; remove or rename the legacy alias")]
    PrefixReserved,
    #[error("alias name 'modules' is reserved; remove or rename the legacy alias")]
    ModulesReserved,
    #[error("alias already exists")]
    AlreadyExists,
    #[error("alias target is not a canonical command")]
    UnknownTarget,
    #[error("alias target is not aliasable")]
    TargetNotAliasable,
    #[error("alias limit exceeded")]
    AliasLimit,
    #[error("alias argument limit exceeded")]
    ArgumentLimit,
    #[error("alias argument is too long")]
    ArgumentTooLong,
    #[error("alias arguments are too large")]
    ArgumentsTooLarge,
    #[error("alias file is too large")]
    FileTooLarge,
    #[error("alias file is malformed")]
    MalformedFile,
    #[error("alias file version is unsupported")]
    UnsupportedVersion,
    #[error("failed to read alias storage")]
    Read,
    #[error("alias state directory is unavailable")]
    CreateDirectory,
    #[error("failed to create alias temporary file")]
    CreateTemporary,
    #[error("failed to write alias temporary file")]
    WriteTemporary,
    #[error("failed to synchronize alias temporary file")]
    SyncTemporary,
    #[error("failed to replace alias file")]
    Replace,
    #[error("failed to run alias storage task")]
    StorageTask,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("Telegram API ID is outside the supported range")]
    InvalidApiId,
    #[error("failed to create the session state directory")]
    CreateSessionDirectory,
    #[error("session path has no parent directory")]
    MissingSessionDirectory,
    #[error("failed to secure the session state directory")]
    SecureSessionDirectory,
    #[error("failed to open the local session database")]
    OpenSession,
    #[error("failed to secure the local session database")]
    SecureSessionFile,
    #[error("Telegram runner task failed")]
    RunnerTask,
    #[error("Telegram update stream has already been started")]
    UpdatesAlreadyTaken,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("Telegram authorization requires an interactive terminal")]
    NonInteractive,
    #[error("failed to read authorization input")]
    ReadInput,
    #[error("authorization input must not be empty")]
    EmptyInput,
    #[error("failed to check Telegram authorization status")]
    AuthorizationCheck,
    #[error("failed to request a Telegram login code")]
    RequestLoginCode,
    #[error("Telegram sign-up must be completed in an official client")]
    SignUpRequired,
    #[error("the Telegram login code was invalid")]
    InvalidCode,
    #[error("the Telegram two-factor password was invalid")]
    InvalidPassword,
    #[error("Telegram sign-in failed")]
    SignIn,
    #[error("failed to retrieve the authorized Telegram account")]
    GetAuthorizedUser,
}
