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
    #[error("neither XDG_STATE_HOME nor HOME is available for the session path")]
    MissingStateDirectory,
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
