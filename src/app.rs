use anyhow::Context;
use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::Instant,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub mod aliases;
pub mod auth;
pub mod bot_api;
pub mod client;
pub mod command;
pub mod commands;
pub mod config;
pub mod credentials;
pub mod error;
pub mod external_modules;
pub mod fastfetch;
pub mod help;
pub mod modules;
pub mod reboot_receipt;
pub mod response;
pub mod runtime;
pub mod settings;
pub mod setup;
pub mod setup_grammers;
pub mod setup_provision;
pub mod setup_store;
pub mod setup_telegram;
pub mod updates;

use auth::AuthorizationOutcome;

struct TelegramClientGuard(Option<client::TelegramClient>);

impl TelegramClientGuard {
    fn new(client: client::TelegramClient) -> Self {
        Self(Some(client))
    }

    fn inner(&mut self) -> &mut client::TelegramClient {
        self.0.as_mut().expect("TelegramClient already taken")
    }

    async fn shutdown(mut self) -> Result<(), ClientError> {
        if let Some(client) = self.0.take() {
            client.shutdown().await
        } else {
            Ok(())
        }
    }
}

impl Drop for TelegramClientGuard {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            tokio::spawn(client.shutdown());
        }
    }
}

use crate::error::AuthError;
use crate::error::ClientError;

pub async fn run() -> anyhow::Result<()> {
    match parse_cli(env::args_os().skip(1))? {
        CliCommand::Run => run_command(false).await,
        CliCommand::Auth => run_command(true).await,
        CliCommand::Credentials => credentials_status().await,
        CliCommand::CredentialsReset => credentials_reset().await,
        CliCommand::Logout => logout().await,
        CliCommand::ModulesValidate { path } => modules_validate(path).await,
        CliCommand::ModulesEnable { id } => modules_enable(id).await,
        CliCommand::ModulesDisable { id } => modules_disable(id).await,
        CliCommand::ModulesStatus => modules_status().await,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Run,
    Auth,
    Credentials,
    CredentialsReset,
    Logout,
    ModulesValidate { path: PathBuf },
    ModulesEnable { id: String },
    ModulesDisable { id: String },
    ModulesStatus,
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
        [modules, subcommand] if modules == "modules" && subcommand == "status" => {
            Ok(CliCommand::ModulesStatus)
        }
        [modules, subcommand, id] if modules == "modules" && subcommand == "enable" => {
            Ok(CliCommand::ModulesEnable {
                id: id.to_string_lossy().into_owned(),
            })
        }
        [modules, subcommand, id] if modules == "modules" && subcommand == "disable" => {
            Ok(CliCommand::ModulesDisable {
                id: id.to_string_lossy().into_owned(),
            })
        }
        [modules, subcommand, path] if modules == "modules" && subcommand == "validate" => {
            Ok(CliCommand::ModulesValidate {
                path: PathBuf::from(path),
            })
        }
        _ => anyhow::bail!(
            "usage: lavis [run|auth|credentials [reset]|logout|modules [validate <path>|enable <id>|disable <id>|status]]"
        ),
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
    let client = client::TelegramClient::connect(&config)
        .await
        .context("failed to open the Telegram session")?;
    let mut guard = TelegramClientGuard::new(client);
    let mut external_handle = None;
    let application_result = async {
        // Load settings early (needed for prefix in quick_start).
        let settings = settings::SettingsStore::load(config.settings_path.clone())
            .await
            .context("failed to load persistent settings")?;
        let prefix = settings.prefix().to_owned();

        let outcome = auth::authorize(guard.inner().client(), &config)
            .await
            .context("Telegram authorization failed")
            .map_err(|error| authorization_failure(error, newly_saved))?;

        if should_show_quick_start(&outcome) {
            let quick_start = render_quick_start(&prefix);
            if let Err(error) = guard
                .inner()
                .client()
                .send_message(
                    &grammers_client::tl::types::InputPeerSelf {},
                    grammers_client::message::InputMessage::new().text(quick_start.clone()),
                )
                .await
            {
                tracing::warn!(
                    event = "quick_start_send_failed",
                    error = %error,
                    "Failed to send post-auth quick start message"
                );
                let fallback = render_quick_start_fallback(&quick_start);
                let _ = writeln!(io::stdout().lock(), "{fallback}");
            }
        }

        if auth_only {
            return Ok(runtime::ShutdownReason::Exit);
        }

        let self_user_id = outcome.self_user_id();
        initialize_dialog_cache(guard.inner().client()).await?;
        let mut stream = {
            let client_ref = guard.inner();
            let receiver = client_ref
                .take_updates()
                .context("failed to start the Telegram update stream")?;
            client_ref
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
                .context("failed to create the Telegram update stream")?
        };

        let aliases = aliases::AliasStore::load(config.aliases_path.clone())
            .await
            .context("failed to load persistent aliases")?;

        // Set up external modules.
        let external_state_path =
            config::ConfigPaths::external_modules_state_path_with(&environment)
                .context("failed to determine external modules state path")?;
        let external_state =
            external_modules::state::ExternalStateStore::load(external_state_path.clone())
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(
                        event = "external_state_load_failed",
                        error = %error,
                        "External modules state unavailable, continuing without them"
                    );
                    external_modules::state::ExternalStateStore::new_disabled()
                });

        let module_root = config::ConfigPaths::data_dir_with(&environment)
            .context("failed to determine data directory")?
            .join(external_modules::MODULE_DIR_NAME);
        let declarative_state_path = external_state_path
            .parent()
            .context("external modules state has no parent")?
            .join("declarative-modules.json");
        fs::create_dir_all(&module_root).context("failed to create external module root")?;
        let module_root_metadata =
            fs::symlink_metadata(&module_root).context("failed to inspect external module root")?;
        if !module_root_metadata.file_type().is_dir()
            || module_root_metadata.file_type().is_symlink()
        {
            anyhow::bail!("external module root is not a safe directory");
        }
        let module_staging_root = module_root
            .parent()
            .context("external module root has no parent")?
            .join("module-staging");
        prepare_module_staging_root(&module_staging_root)?;
        let cleanup_failures =
            external_modules::installer::cleanup_abandoned_wrappers(&module_staging_root)
                .context("failed to clean abandoned external module staging")?;
        for failure in cleanup_failures {
            tracing::warn!(
                event = "external_module_staging_cleanup_failed",
                wrapper = %failure.wrapper.display(),
                ?failure.kind,
                "Could not remove abandoned external module staging"
            );
        }

        let descriptors = external_modules::manifest::discover_modules(&module_root)
            .unwrap_or_else(|error| {
                tracing::warn!(
                    event = "external_discovery_failed",
                    error = %error,
                    "External module discovery failed, continuing without modules"
                );
                Vec::new()
            });

        let external_manager = external_modules::manager::ExternalManager::new();
        let handle = external_modules::manager::ExternalManagerHandle::new(external_manager);
        external_handle = Some(handle.clone());
        {
            let mut mgr = handle.lock().await;
            mgr.set_descriptors(descriptors);
            mgr.set_gateway(external_modules::gateway::GrammersGateway::new(
                guard.inner().client().clone(),
            ));
            mgr.set_v6_executor(external_modules::v6_executor::GrammersV6Executor::new(
                guard.inner().module_rpc_client(),
            ));
        }
        handle.startup_enabled(external_state.enabled_ids()).await;
        let mut runtime = runtime::RuntimeState::new(
            started_at,
            aliases,
            settings,
            config.fastfetch_profile_path.clone(),
        );
        runtime.configure_setup(
            config::ConfigPaths::setup_state_path_with(&environment)
                .context("failed to determine setup state path")?,
            config::ConfigPaths::companion_token_path_with(&environment)
                .context("failed to determine companion token path")?,
            self_user_id,
        );
        runtime.configure_module_installation(
            module_root.clone(),
            module_staging_root,
            self_user_id,
        );
        runtime.configure_module_control(
            module_root,
            external_state_path,
            declarative_state_path,
            self_user_id,
        );
        runtime.set_external_manager(handle).await;
        let receipt_path = config::ConfigPaths::external_modules_state_path_with(&environment)
            .context("failed to determine reboot receipt state path")?
            .parent()
            .context("reboot receipt state has no parent")?
            .join("pending-reboot.json");
        let receipt_store = reboot_receipt::RebootReceiptStore::new(receipt_path);
        tracing::info!(event = "application_started", prefix = %runtime.prefix(), "lavis is running");

        let run_result = {
            let client_ref = guard.inner();
            updates::run(
                &mut stream,
                self_user_id,
                client_ref.client(),
                &mut runtime,
                &receipt_store,
            )
            .await
        };

        runtime.shutdown_module_approvals();
        drop(stream);
        run_result
    }
    .await;

    if let Some(handle) = external_handle {
        // Stop module processes before their core-owned Telegram gateway is
        // disconnected.
        handle.shutdown_all().await;
    }
    let shutdown_result = guard.shutdown().await;

    let reason = combine_application_and_shutdown(application_result, shutdown_result)?;
    if reason == runtime::ShutdownReason::Restart {
        restart_current_process()?;
    }
    Ok(())
}

fn combine_application_and_shutdown(
    application_result: anyhow::Result<runtime::ShutdownReason>,
    shutdown_result: Result<(), ClientError>,
) -> anyhow::Result<runtime::ShutdownReason> {
    match (application_result, shutdown_result) {
        (Ok(reason), Ok(())) => {
            tracing::info!(event = "application_stopped", "lavis stopped");
            Ok(reason)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
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

#[cfg(unix)]
fn restart_current_process() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let executable =
        env::current_exe().context("failed to resolve current executable for restart")?;
    let error = std::process::Command::new(executable)
        .args(env::args_os().skip(1))
        .exec();
    Err(anyhow::Error::new(error).context("failed to exec Lavis restart"))
}

#[cfg(not(unix))]
fn restart_current_process() -> anyhow::Result<()> {
    anyhow::bail!("restart is unsupported on this platform")
}

fn authorization_failure(error: anyhow::Error, newly_saved: bool) -> anyhow::Error {
    let noninteractive = error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<AuthError>(),
            Some(AuthError::NonInteractive)
        )
    });
    let error = if newly_saved {
        error.context(
            "new credentials were saved; if they are incorrect, run `lavis credentials reset`",
        )
    } else {
        error
    };
    if noninteractive {
        error.context(
            "authorize Telegram first with `lavis auth` in an interactive terminal; on NixOS, run `sudo lavis-auth` before starting lavis.service",
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

fn should_show_quick_start(outcome: &AuthorizationOutcome) -> bool {
    outcome.is_just_completed()
}

fn render_quick_start(prefix: &str) -> String {
    format!(
        "✅ Авторизация завершена\n\n\
        Начало работы:\n  {prefix}help\n  {prefix}modules\n  {prefix}help fastfetch\n  {prefix}help alias"
    )
}

fn render_quick_start_fallback(quick_start: &str) -> String {
    format!(
        "Не удалось отправить подсказку в Telegram.\n\n\
        {quick_start}"
    )
}

async fn modules_validate(path: PathBuf) -> anyhow::Result<()> {
    let path = path.canonicalize().context("path does not exist")?;
    match external_modules::manifest::validate_manifest_at(&path, None) {
        Ok(desc) => {
            println!("✅ Модуль «{}» корректен.", desc.display_name);
            println!("   ID: {}", desc.id);
            println!("   Версия: {}", desc.version);
            println!("   Автор: {}", desc.author);
            println!("   Команд: {}", desc.commands.len());
            println!(
                "   Возможности: {}",
                desc.capabilities
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Ok(())
        }
        Err(error) => {
            println!("❌ Ошибка валидации: {error}");
            Err(error.into())
        }
    }
}

async fn modules_enable(id: String) -> anyhow::Result<()> {
    let environment = |name: &str| std::env::var_os(name);
    let module_root = config::ConfigPaths::data_dir_with(&environment)
        .context("failed to determine data directory")?
        .join(external_modules::MODULE_DIR_NAME);
    let state_path = config::ConfigPaths::external_modules_state_path_with(&environment)
        .context("failed to determine state path")?;

    let mut state = external_modules::state::ExternalStateStore::load(state_path.clone())
        .await
        .context("failed to load module state")?;
    let declarative = config::ConfigPaths::external_modules_state_path_with(&environment)?
        .parent()
        .context("state path has no parent")?
        .join("declarative-modules.json");
    let operation =
        external_modules::control::enable_module(&module_root, &declarative, &mut state, &id)
            .await?;
    println!(
        "{} Модуль «{}» {}. Требуется перезапуск lavis.",
        if operation.changed { "✅" } else { "ℹ️" },
        operation.module.display_name,
        if operation.changed {
            "включён"
        } else {
            "уже включён"
        }
    );
    Ok(())
}

async fn modules_disable(id: String) -> anyhow::Result<()> {
    let environment = |name: &str| std::env::var_os(name);
    let state_path = config::ConfigPaths::external_modules_state_path_with(&environment)
        .context("failed to determine state path")?;

    let mut state = external_modules::state::ExternalStateStore::load(state_path.clone())
        .await
        .context("failed to load module state")?;
    let module_root =
        config::ConfigPaths::data_dir_with(&environment)?.join(external_modules::MODULE_DIR_NAME);
    let declarative = state_path
        .parent()
        .context("state path has no parent")?
        .join("declarative-modules.json");
    let operation =
        external_modules::control::disable_module(&module_root, &declarative, &mut state, &id)
            .await?;
    println!(
        "{} Модуль «{}» {}. Требуется перезапуск lavis.",
        if operation.changed { "✅" } else { "ℹ️" },
        operation.module.display_name,
        if operation.changed {
            "отключён"
        } else {
            "уже отключён"
        }
    );
    Ok(())
}

async fn modules_status() -> anyhow::Result<()> {
    let environment = |name: &str| std::env::var_os(name);
    let module_root = config::ConfigPaths::data_dir_with(&environment)
        .context("failed to determine data directory")?
        .join(external_modules::MODULE_DIR_NAME);
    let state_path = config::ConfigPaths::external_modules_state_path_with(&environment)
        .context("failed to determine state path")?;

    let state = external_modules::state::ExternalStateStore::load(state_path.clone())
        .await
        .context("failed to load module state")?;
    let declarative = state_path
        .parent()
        .context("state path has no parent")?
        .join("declarative-modules.json");
    let list = external_modules::control::list_modules(&module_root, &declarative, &state)?;
    if list.modules.is_empty() && state.enabled_ids().is_empty() {
        println!("Внешние модули не обнаружены.");
        return Ok(());
    }

    println!("Внешние модули (альфа):");
    for entry in &list.modules {
        match &entry.module {
            Some(module) => println!(
                "  • {} ({}) — v{}, автор: {} — {}, команд: {}",
                module.display_name,
                module.id,
                module.version,
                module.author,
                if module.enabled {
                    "включён"
                } else {
                    "отключён"
                },
                module.commands.len()
            ),
            None => println!(
                "  • {} — диагностика: {:?}",
                entry.id.as_deref().unwrap_or("<некорректный ID>"),
                entry.diagnostic
            ),
        }
    }
    for id in state.enabled_ids() {
        if !list
            .modules
            .iter()
            .any(|entry| entry.id.as_deref() == Some(id))
        {
            println!("  • {id} — включён, но манифест не найден");
        }
    }

    println!();
    println!("⚠️ Внешние модули запускаются отдельными процессами с правами вашего пользователя.");
    println!("   Lavis не помещает их в системную песочницу. Включайте только доверенные модули.");
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

fn prepare_module_staging_root(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                anyhow::bail!("external module staging root is not a safe directory");
            }
            #[cfg(unix)]
            if metadata.permissions().mode() & 0o077 != 0 {
                anyhow::bail!("external module staging root has insecure permissions");
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).context("failed to create external module staging root")?;
        }
        Err(error) => return Err(error).context("failed to inspect external module staging root"),
    }

    let metadata =
        fs::symlink_metadata(path).context("failed to verify external module staging root")?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        anyhow::bail!("external module staging root is not a safe directory");
    }
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .context("failed to secure external module staging root")?;
        if fs::metadata(path)
            .context("failed to verify external module staging root permissions")?
            .permissions()
            .mode()
            & 0o777
            != 0o700
        {
            anyhow::bail!("external module staging root permissions are not secure");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::prepare_module_staging_root;
    use super::{
        AuthorizationOutcome, CliCommand, ClientError, NONINTERACTIVE_LOGOUT,
        NONINTERACTIVE_MISSING_CREDENTIALS, authorization_failure,
        combine_application_and_shutdown, logout_confirmed, parse_cli, remove_session_files,
        render_quick_start, render_quick_start_fallback, should_show_quick_start,
    };
    use std::{
        ffi::OsString,
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

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

    #[cfg(unix)]
    #[test]
    fn staging_root_is_private_and_rejects_symlinks_and_insecure_directories() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-staging-root-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let staging_root = directory.join("staging");
        prepare_module_staging_root(&staging_root).unwrap();
        assert_eq!(
            fs::metadata(&staging_root).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let insecure = directory.join("insecure");
        fs::create_dir(&insecure).unwrap();
        fs::set_permissions(&insecure, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prepare_module_staging_root(&insecure).is_err());

        let target = directory.join("target");
        fs::create_dir(&target).unwrap();
        let link = directory.join("link");
        symlink(&target, &link).unwrap();
        assert!(prepare_module_staging_root(&link).is_err());
        fs::remove_dir_all(directory).unwrap();
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
    fn authorization_failure_explains_noninteractive_service_recovery() {
        let error = authorization_failure(
            anyhow::Error::new(crate::error::AuthError::NonInteractive)
                .context("Telegram authorization failed"),
            false,
        );

        assert!(error.to_string().contains("sudo lavis-auth"));
        assert!(
            error.chain().any(|cause| cause.to_string()
                == "Telegram authorization requires an interactive terminal")
        );
    }

    #[test]
    fn shutdown_result_preserves_the_application_error() {
        let error = combine_application_and_shutdown(
            Err(anyhow::anyhow!("application failed")),
            Err(ClientError::RunnerTask),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Telegram runner shutdown also failed")
        );
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string() == "application failed")
        );
    }

    #[test]
    fn shutdown_error_is_returned_when_application_succeeds() {
        let error = combine_application_and_shutdown(
            Ok(crate::runtime::ShutdownReason::Exit),
            Err(ClientError::RunnerTask),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Telegram runner task failed"));
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

    #[test]
    fn should_show_quick_start_is_true_for_just_completed() {
        use grammers_session::types::PeerId;
        let outcome = AuthorizationOutcome::JustCompleted {
            self_user_id: PeerId::self_user(),
        };
        assert!(should_show_quick_start(&outcome));
    }

    #[test]
    fn should_show_quick_start_is_false_for_existing_session() {
        use grammers_session::types::PeerId;
        let outcome = AuthorizationOutcome::ExistingSession {
            self_user_id: PeerId::self_user(),
        };
        assert!(!should_show_quick_start(&outcome));
    }

    #[test]
    fn render_quick_start_uses_non_default_prefix() {
        let text = render_quick_start("🦀");
        assert!(text.contains("🦀help"));
        assert!(text.contains("🦀modules"));
        assert!(text.contains("🦀help fastfetch"));
        assert!(text.contains("🦀help alias"));
        assert!(!text.contains(",help"));
    }

    #[test]
    fn render_quick_start_preserves_russian_and_emoji() {
        let text = render_quick_start(",");
        assert!(text.contains("✅"));
        assert!(text.contains("Авторизация завершена"));
        assert!(text.contains("Начало работы"));
    }

    #[test]
    fn render_quick_start_contains_no_sensitive_data() {
        let text = render_quick_start(",");
        assert!(!text.contains("/home/"));
        assert!(!text.contains("api_id"));
        assert!(!text.contains("api_hash"));
        assert!(!text.contains("session"));
        assert!(!text.contains("credentials"));
    }

    #[test]
    fn render_quick_start_fallback_is_russian_and_includes_text() {
        let inner = render_quick_start(",");
        let fallback = render_quick_start_fallback(&inner);
        assert!(fallback.starts_with("Не удалось отправить подсказку в Telegram."));
        assert!(fallback.contains(inner.as_str()));
        assert!(fallback.contains("✅"));
        assert!(fallback.contains("Авторизация завершена"));
    }
}
