use anyhow::Context;
use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, IsTerminal, Write},
    path::Path,
    time::Instant,
};

pub mod aliases;
pub mod auth;
pub mod client;
pub mod command;
pub mod commands;
pub mod config;
pub mod credentials;
pub mod error;
pub mod fastfetch;
pub mod help;
pub mod modules;
pub mod response;
pub mod runtime;
pub mod settings;
pub mod updates;

pub async fn run() -> anyhow::Result<()> {
    match parse_cli(env::args_os().skip(1))? {
        CliCommand::Run => run_command(false).await,
        CliCommand::Auth => run_command(true).await,
        CliCommand::Credentials => credentials_status().await,
        CliCommand::CredentialsReset => credentials_reset().await,
        CliCommand::Logout => logout().await,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliCommand {
    Run,
    Auth,
    Credentials,
    CredentialsReset,
    Logout,
}

const NONINTERACTIVE_MISSING_CREDENTIALS: &str =
    "Run `lavis auth` in an interactive terminal first.";
const NONINTERACTIVE_LOGOUT: &str = "logout requires an interactive terminal";

fn parse_cli(arguments: impl IntoIterator<Item = OsString>) -> anyhow::Result<CliCommand> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => Ok(CliCommand::Run),
        [argument] if argument == "run" => Ok(CliCommand::Run),
        [argument] if argument == "auth" => Ok(CliCommand::Auth),
        [argument] if argument == "credentials" => Ok(CliCommand::Credentials),
        [credentials, reset] if credentials == "credentials" && reset == "reset" => {
            Ok(CliCommand::CredentialsReset)
        }
        [argument] if argument == "logout" => Ok(CliCommand::Logout),
        _ => anyhow::bail!("usage: lavis [run|auth|credentials [reset]|logout]"),
    }
}

async fn run_command(auth_only: bool) -> anyhow::Result<()> {
    let started_at = Instant::now();
    let environment = |name: &str| std::env::var_os(name);
    let resolved = resolve_or_onboard(&environment).await?;
    let newly_saved = resolved.newly_saved;
    let paths = config::ConfigPaths::default_with(&environment)
        .context("failed to determine application paths")?;
    let config = config::Config::from_credentials(resolved.credentials, paths)
        .context("failed to load configuration")?;
    let mut client = client::TelegramClient::connect(&config)
        .await
        .context("failed to open the Telegram session")?;

    let run_result = async {
        let (self_user_id, just_authorized) = auth::authorize(client.client(), &config)
            .await
            .context("Telegram authorization failed")
            .map_err(|error| authorization_failure(error, newly_saved))?;

        if just_authorized {
            let prefix = settings::SettingsStore::load(config.settings_path.clone())
                .await
                .context("failed to load prefix for quick start")?
                .prefix()
                .to_owned();
            let quick_start = format!(
                "✅ Авторизация завершена\n\n\
                Начало работы:\n  {prefix}help\n  {prefix}modules\n  {prefix}help fastfetch\n  {prefix}help alias"
            );
            let input = grammers_client::message::InputMessage::new().text(quick_start);
            if let Err(error) = client
                .client()
                .send_message(
                    &grammers_client::tl::types::InputPeerSelf {},
                    input,
                )
                .await
            {
                tracing::warn!(
                    event = "quick_start_send_failed",
                    error = %error,
                    "Failed to send post-auth quick start message"
                );
            }
        }

        if auth_only {
            return Ok(());
        }
        let settings = settings::SettingsStore::load(config.settings_path.clone())
            .await
            .context("failed to load persistent settings")?;
        initialize_dialog_cache(client.client()).await?;
        let receiver = client
            .take_updates()
            .context("failed to start the Telegram update stream")?;
        let mut stream = client
            .client()
            .stream_updates(
                receiver,
                grammers_client::client::UpdatesConfiguration {
                    catch_up: false,
                    ..Default::default()
                },
            )
            .await
            .map_err(anyhow::Error::from_boxed)
            .context("failed to create the Telegram update stream")?;

        let aliases = aliases::AliasStore::load(config.aliases_path.clone())
            .await
            .context("failed to load persistent aliases")?;
        tracing::info!(event = "application_started", "lavis is running");
        let mut runtime = runtime::RuntimeState::new(
            started_at,
            aliases,
            settings,
            config.fastfetch_profile_path.clone(),
        );
        updates::run(&mut stream, self_user_id, client.client(), &mut runtime).await?;
        drop(stream);
        Ok(())
    }
    .await;

    let shutdown_result = client.shutdown().await;
    match (run_result, shutdown_result) {
        (Ok(()), Ok(())) => {
            tracing::info!(event = "application_stopped", "lavis stopped");
            Ok(())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(shutdown_error)) => {
            tracing::error!(
                event = "application_shutdown_failed",
                %shutdown_error,
                "Telegram runner shutdown failed"
            );
            Err(error.context("Telegram runner shutdown also failed"))
        }
    }
}

fn authorization_failure(error: anyhow::Error, newly_saved: bool) -> anyhow::Error {
    if newly_saved {
        error.context(
            "new credentials were saved; if they are incorrect, run `lavis credentials reset`",
        )
    } else {
        error
    }
}

struct ResolvedCredentials {
    credentials: credentials::Credentials,
    newly_saved: bool,
}

async fn resolve_or_onboard<F>(environment: &F) -> anyhow::Result<ResolvedCredentials>
where
    F: Fn(&str) -> Option<OsString>,
{
    match credentials::resolve_environment(environment)? {
        Some(credentials) => Ok(ResolvedCredentials {
            credentials,
            newly_saved: false,
        }),
        None => {
            let path =
                credentials::credentials_path(config::ConfigPaths::config_dir_with(environment)?);
            match tokio::task::spawn_blocking(move || credentials::resolve_stored(path))
                .await
                .map_err(|_| anyhow::anyhow!("credential storage task failed"))?
            {
                Ok((credentials, _)) => Ok(ResolvedCredentials {
                    credentials,
                    newly_saved: false,
                }),
                Err(crate::error::CredentialsError::NotFound) if credentials::interactive() => {
                    let path = credentials::credentials_path(config::ConfigPaths::config_dir_with(
                        environment,
                    )?);
                    let credentials =
                        tokio::task::spawn_blocking(move || credentials::onboard(path))
                            .await
                            .map_err(|_| anyhow::anyhow!("credential onboarding task failed"))?
                            .context("credential onboarding failed")?;
                    Ok(ResolvedCredentials {
                        credentials,
                        newly_saved: true,
                    })
                }
                Err(crate::error::CredentialsError::NotFound) => {
                    anyhow::bail!(NONINTERACTIVE_MISSING_CREDENTIALS)
                }
                Err(error) => Err(error).context("failed to resolve credentials"),
            }
        }
    }
}

async fn credentials_reset() -> anyhow::Result<()> {
    if !credentials::interactive() {
        anyhow::bail!("credentials reset requires an interactive terminal")
    }
    let confirmed = tokio::task::spawn_blocking(read_credentials_reset_confirmation)
        .await
        .map_err(|_| anyhow::anyhow!("credentials reset confirmation task failed"))??;
    if !confirmed {
        anyhow::bail!("credentials reset cancelled")
    }
    let environment = |name: &str| std::env::var_os(name);
    let path = credentials::credentials_path(config::ConfigPaths::config_dir_with(&environment)?);
    let result = tokio::task::spawn_blocking(move || credentials::reset(&path))
        .await
        .map_err(|_| anyhow::anyhow!("credentials reset storage task failed"))??;
    match result {
        credentials::ResetResult::Removed => println!("Local credentials removed."),
        credentials::ResetResult::Absent => println!("No local credentials were present."),
    }
    Ok(())
}

fn read_credentials_reset_confirmation() -> io::Result<bool> {
    print!("Remove local API credentials? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(credentials::confirmed(&answer))
}

async fn credentials_status() -> anyhow::Result<()> {
    let environment = |name: &str| std::env::var_os(name);
    if credentials::resolve_environment(&environment)?.is_some() {
        println!("credentials: present (Environment); path: not used");
        return Ok(());
    }
    let config_dir = config::ConfigPaths::config_dir_with(&environment)?;
    let path = credentials::credentials_path(config_dir);
    let path_display = path.display().to_string();
    let result = tokio::task::spawn_blocking(move || credentials::resolve_stored(path))
        .await
        .map_err(|_| anyhow::anyhow!("credential storage task failed"))?;
    match result {
        Ok((_, source)) => println!("credentials: present ({source:?}); path: {path_display}"),
        Err(crate::error::CredentialsError::NotFound) => {
            println!("credentials: absent; path: {path_display}")
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

async fn logout() -> anyhow::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(NONINTERACTIVE_LOGOUT)
    }
    let confirmed = tokio::task::spawn_blocking(read_logout_confirmation)
        .await
        .map_err(|_| anyhow::anyhow!("logout confirmation task failed"))??;
    if !confirmed {
        anyhow::bail!("logout cancelled")
    }
    let environment = |name: &str| std::env::var_os(name);
    let session_path = config::ConfigPaths::state_session_path_with(&environment)?;
    tokio::task::spawn_blocking(move || remove_session_files(&session_path))
        .await
        .map_err(|_| anyhow::anyhow!("logout storage task failed"))??;
    println!("Local Telegram session removed. This does not revoke remote access.");
    Ok(())
}

fn read_logout_confirmation() -> io::Result<bool> {
    print!("Remove the local Telegram session? This does not revoke remote access. [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(logout_confirmed(&answer))
}

fn logout_confirmed(answer: &str) -> bool {
    credentials::confirmed(answer)
}

fn remove_session_files(session: &Path) -> anyhow::Result<()> {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut path = session.to_path_buf();
        path.as_mut_os_string().push(suffix);
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn initialize_dialog_cache(client: &grammers_client::Client) -> anyhow::Result<()> {
    let mut dialogs = client.iter_dialogs();
    while dialogs
        .next()
        .await
        .context("failed to initialize the Telegram dialog cache")?
        .is_some()
    {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CliCommand, NONINTERACTIVE_LOGOUT, NONINTERACTIVE_MISSING_CREDENTIALS,
        authorization_failure, logout_confirmed, parse_cli, remove_session_files,
    };
    use std::{
        ffi::OsString,
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn accepts_only_documented_cli_forms() {
        assert_eq!(parse_cli(Vec::new()).unwrap(), CliCommand::Run);
        assert_eq!(
            parse_cli(vec![OsString::from("run")]).unwrap(),
            CliCommand::Run
        );
        assert_eq!(
            parse_cli(vec![OsString::from("auth")]).unwrap(),
            CliCommand::Auth
        );
        assert_eq!(
            parse_cli(vec![OsString::from("credentials")]).unwrap(),
            CliCommand::Credentials
        );
        assert_eq!(
            parse_cli(vec![OsString::from("credentials"), OsString::from("reset"),]).unwrap(),
            CliCommand::CredentialsReset
        );
        assert_eq!(
            parse_cli(vec![OsString::from("logout")]).unwrap(),
            CliCommand::Logout
        );
        assert!(parse_cli(vec![OsString::from("auth"), OsString::from("extra")]).is_err());
        assert!(
            parse_cli(vec![
                OsString::from("credentials"),
                OsString::from("reset"),
                OsString::from("extra"),
            ])
            .is_err()
        );
        assert!(parse_cli(vec![OsString::from("unknown")]).is_err());
    }

    #[test]
    fn noninteractive_missing_credentials_message_is_exact() {
        assert_eq!(
            NONINTERACTIVE_MISSING_CREDENTIALS,
            "Run `lavis auth` in an interactive terminal first."
        );
    }

    #[test]
    fn noninteractive_logout_message_is_exact() {
        assert_eq!(
            NONINTERACTIVE_LOGOUT,
            "logout requires an interactive terminal"
        );
    }

    #[test]
    fn authorization_recovery_hint_only_applies_to_newly_saved_credentials() {
        let new = authorization_failure(anyhow::anyhow!("authorization failed"), true);
        let existing = authorization_failure(anyhow::anyhow!("authorization failed"), false);

        assert!(new.to_string().contains("lavis credentials reset"));
        assert!(!existing.to_string().contains("lavis credentials reset"));
    }

    #[test]
    fn logout_confirmation_defaults_to_no() {
        assert!(!logout_confirmed(""));
        assert!(!logout_confirmed("no"));
        assert!(logout_confirmed("yes"));
    }

    #[test]
    fn logout_removes_only_session_and_sidecars() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-logout-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let session = directory.join("session");
        for suffix in ["", "-journal", "-wal", "-shm"] {
            fs::write(format!("{}{}", session.display(), suffix), "session").unwrap();
        }
        for name in ["settings.json", "aliases.json", "credentials.json"] {
            fs::write(directory.join(name), "persistent").unwrap();
        }
        remove_session_files(&session).unwrap();
        for suffix in ["", "-journal", "-wal", "-shm"] {
            assert!(!std::path::PathBuf::from(format!("{}{}", session.display(), suffix)).exists());
        }
        for name in ["settings.json", "aliases.json", "credentials.json"] {
            assert_eq!(
                fs::read_to_string(directory.join(name)).unwrap(),
                "persistent"
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }
}
