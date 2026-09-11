use anyhow::Context;
use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

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
pub mod i18n;
pub mod info;
pub mod log_forwarder;
pub mod message_provenance;
pub mod modules;
pub mod onboarding;
pub mod reboot_receipt;
pub mod response;
pub mod runtime;
pub mod session;
pub mod settings;
pub mod setup;
pub mod setup_grammers;
pub mod setup_provision;
pub mod setup_store;
pub mod setup_telegram;
pub mod updates;
pub mod upstream;

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
        CliCommand::AuthDoctor => auth_doctor().await,
        CliCommand::AuthResetBackup => auth_reset_backup().await,
        CliCommand::Credentials => credentials_status().await,
        CliCommand::CredentialsReset => credentials_reset().await,
        CliCommand::Logout => logout().await,
        CliCommand::ModulesValidate { path } => modules_validate(path).await,
        CliCommand::ModulesEnable { id } => modules_enable(id).await,
        CliCommand::ModulesDisable { id } => modules_disable(id).await,
        CliCommand::ModulesStatus => modules_status().await,
        CliCommand::ValidatePrefix { prefix } => validate_prefix_command(&prefix),
    }
}

/// Whether the failure is terminal for the current local session and requires
/// interactive manual recovery (reauthorization or a session reset). Retrying
/// cannot recover these cases, so the service must not restart-loop on them.
/// Transient transport/RPC failures are deliberately not classified here.
pub fn requires_manual_recovery(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if let Some(AuthError::AuthorizationCheck(failure)) = cause.downcast_ref::<AuthError>() {
            return failure.is_auth_key_duplicated();
        }
        matches!(
            cause.downcast_ref::<ClientError>(),
            Some(ClientError::MalformedSession)
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Run,
    Auth,
    AuthDoctor,
    AuthResetBackup,
    Credentials,
    CredentialsReset,
    Logout,
    ModulesValidate { path: PathBuf },
    ModulesEnable { id: String },
    ModulesDisable { id: String },
    ModulesStatus,
    ValidatePrefix { prefix: String },
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
        [auth, doctor] if auth == "auth" && doctor == "doctor" => Ok(CliCommand::AuthDoctor),
        [auth, reset, backup] if auth == "auth" && reset == "reset" && backup == "--backup" => {
            Ok(CliCommand::AuthResetBackup)
        }
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
        [validate, prefix] if validate == "validate-prefix" => Ok(CliCommand::ValidatePrefix {
            prefix: prefix.to_string_lossy().into_owned(),
        }),
        _ => anyhow::bail!(
            "usage: lavis [run|auth [doctor|reset --backup]|credentials [reset]|logout|modules [validate <path>|enable <id>|disable <id>|status]|validate-prefix <prefix>]"
        ),
    }
}

/// Scripting interface used by the NixOS module activation script so the
/// declarative `services.lavis.settings.prefix` is validated by the exact same
/// Rust validator that the runtime uses. Exits non-zero on an invalid prefix.
fn validate_prefix_command(prefix: &str) -> anyhow::Result<()> {
    settings::validate_prefix(prefix).map_err(|error| anyhow::anyhow!("invalid prefix: {error}"))
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
    let client = match client::TelegramClient::connect(&config).await {
        Ok(client) => client,
        Err(error) => {
            if matches!(
                &error,
                ClientError::OpenSession | ClientError::MalformedSession
            ) {
                let malformed = matches!(&error, ClientError::MalformedSession);
                let session_path = config.session_path.clone();
                let persisted = tokio::task::spawn_blocking(move || {
                    if malformed || session::has_invalid_sqlite_header(&session_path)? {
                        session::write_last_authorization_diagnostic(
                            &session_path,
                            crate::error::LastAuthorizationDiagnostic::MalformedLocalSession,
                        )?;
                    }
                    Ok::<(), ClientError>(())
                })
                .await;
                if !matches!(persisted, Ok(Ok(()))) {
                    tracing::warn!(
                        event = "authorization_diagnostic_persist_failed",
                        "Could not persist sanitized authorization diagnostic"
                    );
                }
            }
            return Err(anyhow::Error::new(error).context("failed to open the Telegram session"));
        }
    };
    let mut guard = TelegramClientGuard::new(client);
    let mut external_handle = None;
    let application_result = async {
        // Load settings early (needed for prefix in quick_start).
        let settings = settings::SettingsStore::load(config.settings_path.clone())
            .await
            .context("failed to load persistent settings")?;
        let prefix = settings.prefix().to_owned();
        let locale = settings.locale();

        let outcome = match auth::authorize(guard.inner().client(), &config).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let diagnostic = auth::last_authorization_diagnostic(&error);
                if let AuthError::AuthorizationCheck(
                    crate::error::AuthorizationCheckFailure::Rpc {
                        code,
                        symbolic_name,
                    }
                    | crate::error::AuthorizationCheckFailure::AuthKeyDuplicated {
                        code,
                        symbolic_name,
                    },
                ) = &error
                {
                    tracing::warn!(
                        event = "authorization_check_failed",
                        category = diagnostic.category(),
                        rpc_code = *code,
                        rpc_name = %symbolic_name,
                        "Telegram authorization check failed"
                    );
                } else {
                    tracing::warn!(
                        event = "authorization_check_failed",
                        category = diagnostic.category(),
                        "Telegram authorization check failed"
                    );
                }
                let session_path = config.session_path.clone();
                if !matches!(tokio::task::spawn_blocking(move || {
                    session::write_last_authorization_diagnostic(&session_path, diagnostic)
                })
                .await, Ok(Ok(()))) {
                    tracing::warn!(
                        event = "authorization_diagnostic_persist_failed",
                        "Could not persist sanitized authorization diagnostic"
                    );
                }
                return Err(authorization_failure(
                    anyhow::Error::new(error).context("Telegram authorization failed"),
                    newly_saved,
                ));
            }
        };
        let session_path = config.session_path.clone();
        if !matches!(tokio::task::spawn_blocking(move || {
            session::write_last_authorization_diagnostic(
                &session_path,
                crate::error::LastAuthorizationDiagnostic::Resolved,
            )
        })
        .await, Ok(Ok(()))) {
            tracing::warn!(
                event = "authorization_diagnostic_persist_failed",
                "Could not persist sanitized authorization diagnostic"
            );
        }
        if should_show_quick_start(&outcome) {
            let quick_start = render_quick_start(&prefix, locale);
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
                let fallback = render_quick_start_fallback(&quick_start, locale);
                let _ = writeln!(io::stdout().lock(), "{fallback}");
            }
        }

        if auth_only {
            return Ok(runtime::ShutdownReason::Exit);
        }

        let self_user_id = outcome.self_user_id();
        let self_identity = outcome.identity().clone();
        // Warm up the dialog cache in the background. This persists peers and
        // initializes update state for broadcast channels/megagroups, which
        // stream_updates gap recovery requires. Runs concurrently with module
        // startup so it does not block the critical path.
        let dialog_cache_client = guard.inner().client().clone();
        tokio::spawn(async move {
            let started = Instant::now();

            if let Err(error) = initialize_dialog_cache(&dialog_cache_client).await {
                tracing::warn!(
                    event = "dialog_cache_init_failed",
                    %error,
                    "Dialog cache initialization failed"
                );
                return;
            }

            tracing::info!(
                event = "dialog_cache_initialized",
                elapsed_ms = started.elapsed().as_millis(),
                "Dialog cache initialized"
            );
        });
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
        let self_edit_ledger = message_provenance::SharedSelfEditLedger::default();
        let bot_form_ledger = message_provenance::SharedBotFormLedger::default();
        let handle = external_modules::manager::ExternalManagerHandle::new(external_manager);
        external_handle = Some(handle.clone());
        let mut inline_registry: Option<
            Arc<external_modules::bot_send::InlineMenuRegistry>,
        > = None;
        {
            let mut mgr = handle.lock().await;
            mgr.set_descriptors(descriptors);
            mgr.set_gateway(external_modules::gateway::GrammersGateway::new(
                guard.inner().client().clone(),
            ));
            mgr.set_v6_executor(external_modules::v6_executor::GrammersV6Executor::new(
                guard.inner().module_rpc_client(),
            ));
            mgr.set_self_edit_ledger(self_edit_ledger.clone());
            match config::ConfigPaths::setup_state_path_with(&environment).context(
                "failed to determine setup state path",
            ) {
                Ok(setup_state_path) => {
                    let token_path = config::ConfigPaths::companion_token_path_with(&environment)
                        .context("failed to determine companion token path")?;
                    crate::log_forwarder::spawn_worker(
                        setup_state_path.clone(),
                        token_path.clone(),
                    );
                    match external_modules::bot_send::CompanionBotSender::new(
                        setup_state_path.clone(),
                        token_path,
                        guard.inner().client().clone(),
                        bot_form_ledger.clone(),
                    ) {
                        Ok(sender) => {
                            let sender = Arc::new(sender);
                            mgr.set_bot_sender(external_modules::bot_send::arc_sender(&sender));
                            mgr.set_inline_surface(external_modules::bot_send::arc_inline(
                                &sender,
                            ));
                            inline_registry = Some(sender.registry());
                        }
                        Err(error) => tracing::warn!(
                            event = "external_module_bot_sender_unavailable",
                            error = ?error,
                            "Companion bot sender could not be initialized"
                        ),
                    }
                }
                Err(error) => tracing::warn!(
                    event = "external_module_bot_sender_unavailable",
                    error = %error,
                    "Companion bot sender disabled without setup state path"
                ),
            }
        }
        if let Some(inline_registry) = inline_registry {
            let setup_state_path =
                config::ConfigPaths::setup_state_path_with(&environment).context(
                    "failed to determine setup state path",
                )?;
            let token_path = config::ConfigPaths::companion_token_path_with(&environment)
                .context("failed to determine companion token path")?;
            external_modules::bot_updates::spawn(external_modules::bot_updates::BotUpdatesConfig {
                state_path: setup_state_path,
                token_path,
                registry: inline_registry,
                self_user_id: self_user_id.bare_id_unchecked(),
                manager: handle.clone(),
            });
        }
        handle.startup_enabled(external_state.enabled_ids()).await;
        let mut runtime = runtime::RuntimeState::new(started_at, aliases, settings);
        runtime.set_self_edit_ledger(self_edit_ledger);
        runtime.set_bot_form_ledger(bot_form_ledger);
        runtime.set_http_upstream();
        runtime.set_self_identity(self_identity);
        runtime.configure_setup(
            config::ConfigPaths::setup_state_path_with(&environment)
                .context("failed to determine setup state path")?,
            config::ConfigPaths::companion_token_path_with(&environment)
                .context("failed to determine companion token path")?,
            self_user_id,
        );
        // Resolve this on every process start, not only while an interactive
        // setup is active. Until it succeeds RuntimeState fails closed and
        // suppresses external event projection, preventing BotFather replies
        // (including token-shaped text) from reaching third-party modules.
        match setup_telegram::GrammersTelegramSetup::resolve(guard.inner().client()).await {
            Ok((_transport, peer)) => runtime.set_setup_botfather_peer(peer),
            Err(_) => tracing::warn!(
                event = "botfather_peer_resolution_failed",
                "External event projection is disabled until BotFather can be resolved"
            ),
        }
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
    let auth_key_duplicated = error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<AuthError>(),
            Some(AuthError::AuthorizationCheck(failure)) if failure.is_auth_key_duplicated()
        )
    });
    let error = if newly_saved {
        error.context(
            "new credentials were saved; if they are incorrect, run `lavis credentials reset`",
        )
    } else {
        error
    };
    let error = if auth_key_duplicated {
        error.context(
            "Telegram invalidated this authorization key; the current local session cannot retry. Run `lavis auth reset --backup` before authorizing again",
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

async fn auth_doctor() -> anyhow::Result<()> {
    let environment = |name: &str| std::env::var_os(name);
    let session_path = config::ConfigPaths::state_session_path_with(&environment)?;
    let report = tokio::task::spawn_blocking(move || session_doctor_report(&session_path))
        .await
        .map_err(|_| anyhow::anyhow!("session doctor task failed"))??;
    println!("{report}");
    Ok(())
}

async fn auth_reset_backup() -> anyhow::Result<()> {
    require_interactive_session_reset(io::stdin().is_terminal(), io::stdout().is_terminal())?;
    let environment = |name: &str| std::env::var_os(name);
    let session_path = config::ConfigPaths::state_session_path_with(&environment)?;
    let backup = tokio::task::spawn_blocking(move || reset_session_with_backup(&session_path))
        .await
        .map_err(|_| anyhow::anyhow!("session reset task failed"))??;

    match backup {
        Some(path) => println!(
            "Local Telegram session moved to private backup directory: {}",
            path.display()
        ),
        None => println!("No local Telegram session files were found."),
    }
    Ok(())
}

fn require_interactive_session_reset(
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
) -> Result<(), ClientError> {
    if stdin_is_terminal && stdout_is_terminal {
        Ok(())
    } else {
        Err(ClientError::SessionResetNonInteractive)
    }
}

fn session_doctor_report(session_path: &Path) -> Result<String, ClientError> {
    let lock_state = match session::lock_state(session_path) {
        Ok(session::SessionLockState::Locked(Some(holder))) => {
            format!("locked (pid={}, context={})", holder.pid, holder.context)
        }
        Ok(session::SessionLockState::Locked(None)) => "locked".to_owned(),
        Ok(session::SessionLockState::Unlocked) => "unlocked".to_owned(),
        Err(_) => "unavailable".to_owned(),
    };
    let metadata = session_file_metadata(session_path).unwrap_or(SessionFileMetadata {
        file_type: "unavailable",
        mode: "unavailable".to_owned(),
        readable: "unavailable",
        owner: "unavailable".to_owned(),
    });
    let diagnostic = match session::read_last_authorization_diagnostic(session_path) {
        Ok(session::StoredAuthorizationDiagnostic::Absent) => "absent".to_owned(),
        Ok(session::StoredAuthorizationDiagnostic::Present(diagnostic)) => {
            diagnostic.category().to_owned()
        }
        Ok(session::StoredAuthorizationDiagnostic::Invalid) => "invalid".to_owned(),
        Err(_) => "unavailable".to_owned(),
    };
    let sidecars = session_sidecar_status(session_path);
    let execution = if std::env::var_os("LAVIS_SERVICE").is_some() {
        "service"
    } else {
        "interactive"
    };
    let reauthorization = match session::read_last_authorization_diagnostic(session_path) {
        Ok(session::StoredAuthorizationDiagnostic::Present(diagnostic)) => {
            diagnostic.requires_manual_recovery()
        }
        _ => false,
    };
    Ok(format!(
        "session path: {}\nlock state: {lock_state}\nsession file: type={}, mode={}, readable={}, owner={}\nsession sidecars: {sidecars}\nlast authorization diagnostic: {diagnostic}\nreauthorization required: {reauthorization}\nexecution context: {execution}",
        session_path.display(),
        metadata.file_type,
        metadata.mode,
        metadata.readable,
        metadata.owner,
    ))
}

struct SessionFileMetadata {
    file_type: &'static str,
    mode: String,
    readable: &'static str,
    owner: String,
}

fn session_file_metadata(session_path: &Path) -> Result<SessionFileMetadata, ClientError> {
    let metadata = match fs::symlink_metadata(session_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SessionFileMetadata {
                file_type: "absent",
                mode: "not-applicable".to_owned(),
                readable: "not-applicable",
                owner: "not-applicable".to_owned(),
            });
        }
        Err(_) => return Err(ClientError::InspectSession),
    };
    let file_type = metadata.file_type();
    let kind = if file_type.is_file() {
        "regular"
    } else if file_type.is_dir() {
        "directory"
    } else if file_type.is_symlink() {
        "symlink"
    } else {
        "other"
    };
    let readable = if file_type.is_file() {
        match fs::File::open(session_path) {
            Ok(file) => {
                drop(file);
                "yes"
            }
            Err(_) => "no",
        }
    } else {
        "not-applicable"
    };
    #[cfg(unix)]
    let mode = format!("{:04o}", metadata.permissions().mode() & 0o777);
    #[cfg(unix)]
    let owner = format!("uid={},gid={}", metadata.uid(), metadata.gid());
    #[cfg(not(unix))]
    let mode = "not-applicable".to_owned();
    #[cfg(not(unix))]
    let owner = "not-applicable".to_owned();

    Ok(SessionFileMetadata {
        file_type: kind,
        mode,
        readable,
        owner,
    })
}

fn session_sidecar_status(session_path: &Path) -> String {
    ["-journal", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| {
            let mut path = session_path.to_path_buf();
            path.as_mut_os_string().push(suffix);
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_file() => format!("{suffix}=present"),
                Ok(_) => format!("{suffix}=unsafe"),
                Err(error) if error.kind() == io::ErrorKind::NotFound => format!("{suffix}=absent"),
                Err(_) => format!("{suffix}=unavailable"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Guards the `auth reset --backup` file transaction.
///
/// Phase 1 (`Staging`): live session files are moved one by one into the
/// staging backup directory. Every error before the commit point rolls the
/// already-moved files back to their live paths, so the active session is never
/// left half-relocated. If rollback itself fails, the staging directory is
/// deliberately retained — never the only surviving copies removed — because
/// the next reset run recovers it.
///
/// Phase 2 (`Committed`): the staging directory is atomically renamed to its
/// published `.session-backup-*` path. That rename is the commit point; after
/// it, recovery cannot undo the reset, so no destructive rollback is attempted
/// and a durability-sync failure is reported as a warning instead of an error.
struct SessionBackupTransaction {
    backup: PathBuf,
    moved: Vec<(PathBuf, PathBuf)>,
    committed: bool,
}

impl SessionBackupTransaction {
    fn begin(backup: PathBuf) -> Self {
        Self {
            backup,
            moved: Vec::new(),
            committed: false,
        }
    }

    /// Move one live session file into the staging directory. Durability of
    /// the rename is enforced immediately; a failure here (rename or sync)
    /// propagates with the file already recorded so the caller can roll back.
    fn move_live_file(
        &mut self,
        source: PathBuf,
        target: PathBuf,
        parent: &Path,
    ) -> Result<(), ClientError> {
        fs::rename(&source, &target).map_err(|_| ClientError::BackupSessionFile)?;
        self.moved.push((source, target));
        sync_directory(parent)?;
        sync_directory(&self.backup)?;
        Ok(())
    }

    /// Publish the staging directory. The staging -> published rename is the
    /// transaction commit point: once it succeeds the reset is complete and
    /// neither these files nor the published backup may be rolled back, even
    /// if the final parent durability sync fails.
    fn commit(&mut self, parent: &Path) -> Result<PathBuf, ClientError> {
        debug_assert!(!self.committed);
        sync_directory(&self.backup)?;
        sync_directory(parent)?;
        let published = published_backup_path(&self.backup)?;
        fs::rename(&self.backup, &published).map_err(|_| ClientError::BackupSessionFile)?;
        self.committed = true;
        if sync_directory(parent).is_err() {
            tracing::warn!(
                event = "session_backup_durability_unsure",
                backup = %published.display(),
                "Session backup was published but its directory durability could not be confirmed"
            );
        }
        Ok(published)
    }

    /// Best-effort rollback of every moved file back to its live path, then
    /// removal of the now-empty staging directory. Only valid before the
    /// commit point. On an incomplete rollback the staging directory is kept
    /// so the next reset run recovers it via `recover_staged_session_backups`.
    fn rollback(&mut self) {
        debug_assert!(!self.committed);
        let mut incomplete = false;
        for (source, target) in self.moved.drain(..).rev() {
            if fs::rename(&target, &source).is_err() {
                incomplete = true;
                break;
            }
        }
        if incomplete {
            tracing::warn!(
                event = "session_backup_rollback_incomplete",
                staging = %self.backup.display(),
                "Could not fully roll back the session reset; the next `auth reset --backup` run will recover the staging directory"
            );
            return;
        }
        if fs::remove_dir(&self.backup).is_err() {
            tracing::warn!(
                event = "session_backup_rollback_left_empty_directory",
                staging = %self.backup.display(),
                "An empty staging directory remains and can be removed manually"
            );
        }
    }
}

fn reset_session_with_backup(session_path: &Path) -> Result<Option<PathBuf>, ClientError> {
    let lock = session::SessionLock::acquire(session_path, session::SessionLockContext::Reset)?;
    let parent = session_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or(ClientError::MissingSessionDirectory)?;
    recover_staged_session_backups(parent, session_path)?;
    let files = session_files_for_backup(session_path)?;
    if files.is_empty() {
        drop(lock);
        return Ok(None);
    }
    let backup = create_session_backup_directory(parent)?;
    let mut transaction = SessionBackupTransaction::begin(backup);
    for source in files {
        let name = source.file_name().ok_or(ClientError::InvalidSessionFile)?;
        let target = transaction.backup.join(name);
        if let Err(error) = transaction.move_live_file(source, target, parent) {
            transaction.rollback();
            return Err(error);
        }
    }
    let published = match transaction.commit(parent) {
        Ok(published) => published,
        Err(error) => {
            transaction.rollback();
            return Err(error);
        }
    };
    drop(lock);
    Ok(Some(published))
}

fn session_files_for_backup(session_path: &Path) -> Result<Vec<PathBuf>, ClientError> {
    let mut files = Vec::new();
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut path = session_path.to_path_buf();
        path.as_mut_os_string().push(suffix);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => files.push(path),
            Ok(_) => return Err(ClientError::InvalidSessionFile),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(ClientError::InspectSession),
        }
    }
    Ok(files)
}

fn create_session_backup_directory(parent: &Path) -> Result<PathBuf, ClientError> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ClientError::CreateSessionBackup)?;
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let backup = parent.join(format!(
        ".session-backup-{}-{}-{sequence}.staging",
        timestamp.as_secs(),
        timestamp.subsec_nanos()
    ));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(&backup)
        .map_err(|_| ClientError::CreateSessionBackup)?;
    #[cfg(unix)]
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o700))
        .map_err(|_| ClientError::CreateSessionBackup)?;
    Ok(backup)
}

fn published_backup_path(staging: &Path) -> Result<PathBuf, ClientError> {
    let parent = staging.parent().ok_or(ClientError::CreateSessionBackup)?;
    let name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix('.'))
        .and_then(|name| name.strip_suffix(".staging"))
        .ok_or(ClientError::CreateSessionBackup)?;
    Ok(parent.join(name))
}

fn recover_staged_session_backups(parent: &Path, session_path: &Path) -> Result<(), ClientError> {
    let session_name = session_path
        .file_name()
        .ok_or(ClientError::MissingSessionDirectory)?;
    // Preflight phase: fully validate every staging directory and every child
    // before any mutation. A conflicting target or an unexpected entry must
    // abort recovery with nothing moved, so a deterministic partial state can
    // never be produced by this function.
    let mut staged = Vec::new();
    for entry in fs::read_dir(parent).map_err(|_| ClientError::BackupSessionFile)? {
        let entry = entry.map_err(|_| ClientError::BackupSessionFile)?;
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if !name_text.starts_with(".session-backup-") || !name_text.ends_with(".staging") {
            continue;
        }
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|_| ClientError::BackupSessionFile)?;
        if !metadata.file_type().is_dir() {
            return Err(ClientError::BackupSessionFile);
        }
        let mut children = Vec::new();
        for child in fs::read_dir(entry.path()).map_err(|_| ClientError::BackupSessionFile)? {
            let child = child.map_err(|_| ClientError::BackupSessionFile)?;
            let child_name = child.file_name();
            let allowed = ["", "-journal", "-wal", "-shm"].iter().any(|suffix| {
                let mut expected = session_name.to_os_string();
                expected.push(suffix);
                child_name == expected
            });
            // `symlink_metadata` instead of `DirEntry::metadata()`: the latter
            // follows symlinks, and a symlink must never be accepted as a
            // valid session/staging file nor renamed out of the expected tree.
            if !allowed
                || !fs::symlink_metadata(child.path())
                    .map_err(|_| ClientError::BackupSessionFile)?
                    .file_type()
                    .is_file()
            {
                return Err(ClientError::BackupSessionFile);
            }
            let target = parent.join(&child_name);
            // `symlink_metadata` also detects dangling symlinks, which
            // `Path::exists()` would miss.
            if fs::symlink_metadata(&target).is_ok() {
                return Err(ClientError::BackupSessionFile);
            }
            children.push((child.path(), target));
        }
        staged.push((entry.path(), children));
    }
    // Mutation phase: move children, rolling back already-moved files if a
    // non-deterministic rename failure occurs mid-way.
    for (staging_dir, children) in staged {
        let mut moved = Vec::new();
        for (source, target) in children {
            if fs::rename(&source, &target).is_err() {
                for (source, target) in moved.into_iter().rev() {
                    fs::rename(target, source).map_err(|_| ClientError::BackupSessionFile)?;
                }
                return Err(ClientError::BackupSessionFile);
            }
            moved.push((source, target));
        }
        fs::remove_dir(&staging_dir).map_err(|_| ClientError::BackupSessionFile)?;
        sync_directory(parent)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), ClientError> {
    #[cfg(test)]
    {
        let scope = if path.display().to_string().contains(".session-backup-") {
            test_hooks::SyncScope::Backup
        } else {
            test_hooks::SyncScope::Parent
        };
        if test_hooks::should_fail_sync(path, scope) {
            return Err(ClientError::BackupSessionFile);
        }
    }
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ClientError::BackupSessionFile)
}

/// Narrow fault injector for `sync_directory`, compiled only into the test
/// harness. It lets the transaction tests fail durability at a precise logical
/// stage (parent vs staging backup dir) without a filesystem abstraction.
/// Faults are per-test-thread and keyed to the reset's directory: `sync_directory`
/// runs synchronously on the test thread, so tests never observe each other's
/// injected failures when they run in parallel.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum SyncScope {
        /// Durability syncs of the live session directory.
        Parent,
        /// Durability syncs of the `.session-backup-*.staging` directory.
        Backup,
    }

    struct SyncFault {
        root: PathBuf,
        scope: SyncScope,
        /// Matched calls in `scope` under `root` already seen. The fault fires
        /// when this reaches zero, i.e. the `ordinal`-th (1-based) call fails
        /// exactly once.
        remaining: u32,
    }

    thread_local! {
        static SYNC_FAULT: RefCell<Option<SyncFault>> = const { RefCell::new(None) };
    }

    /// Make the `ordinal`-th (1-based) `sync_directory` call in `scope`
    /// belonging to the reset rooted at `root` fail. Calls outside the reset's
    /// directory tree and at any other ordinal are unaffected.
    pub(crate) fn fail_next_sync_in(root: &Path, scope: SyncScope, ordinal: u32) {
        SYNC_FAULT.with(|fault| {
            *fault.borrow_mut() = Some(SyncFault {
                root: root.to_path_buf(),
                scope,
                remaining: ordinal,
            });
        });
    }

    pub(crate) fn should_fail_sync(path: &Path, scope: SyncScope) -> bool {
        SYNC_FAULT.with(|fault| {
            let mut fault = fault.borrow_mut();
            let Some(state) = fault.as_mut() else {
                return false;
            };
            if !path.starts_with(&state.root) || state.scope != scope || state.remaining == 0 {
                return false;
            }
            state.remaining -= 1;
            if state.remaining > 0 {
                return false;
            }
            *fault = None;
            true
        })
    }
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
    let lock = session::SessionLock::acquire(session, session::SessionLockContext::Logout)?;
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut path = session.to_path_buf();
        path.as_mut_os_string().push(suffix);
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    drop(lock);
    Ok(())
}

fn should_show_quick_start(outcome: &AuthorizationOutcome) -> bool {
    outcome.is_just_completed()
}

fn render_quick_start(prefix: &str, locale: Option<i18n::Locale>) -> String {
    match locale {
        Some(locale) => i18n::text(locale, i18n::Text::PostAuthInvite).replace("{prefix}", prefix),
        None => i18n::bilingual(i18n::Text::PostAuthInvite, prefix),
    }
}

fn render_quick_start_fallback(quick_start: &str, locale: Option<i18n::Locale>) -> String {
    format!(
        "{}\n\n{quick_start}",
        match locale {
            Some(locale) => i18n::text(locale, i18n::Text::PostAuthFallback).to_owned(),
            None => i18n::bilingual(i18n::Text::PostAuthFallback, ""),
        }
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
        NONINTERACTIVE_MISSING_CREDENTIALS, SessionBackupTransaction, authorization_failure,
        combine_application_and_shutdown, logout_confirmed, parse_cli, remove_session_files,
        render_quick_start, render_quick_start_fallback, require_interactive_session_reset,
        requires_manual_recovery, reset_session_with_backup, session_doctor_report,
        should_show_quick_start, test_hooks,
    };
    use crate::error::AuthorizationCheckFailure;
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
            parse_cli(vec![OsString::from("auth"), OsString::from("doctor")]).unwrap(),
            CliCommand::AuthDoctor
        );
        assert_eq!(
            parse_cli(vec![
                OsString::from("auth"),
                OsString::from("reset"),
                OsString::from("--backup"),
            ])
            .unwrap(),
            CliCommand::AuthResetBackup
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
        assert!(parse_cli(vec![OsString::from("auth"), OsString::from("reset")]).is_err());
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
    fn doctor_reports_only_safe_session_metadata() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-auth-doctor-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");

        let report = session_doctor_report(&session).unwrap();
        assert!(report.contains(&format!("session path: {}", session.display())));
        assert!(report.contains("lock state: unlocked"));
        assert!(report.contains("type=absent, mode=not-applicable, readable=not-applicable"));
        assert!(report.contains("last authorization diagnostic: absent"));
        assert!(!report.contains("api_hash"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn doctor_exposes_the_last_sanitized_authorization_diagnostic_offline() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-auth-doctor-diagnostic-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        crate::session::write_last_authorization_diagnostic(
            &session,
            crate::error::LastAuthorizationDiagnostic::Timeout,
        )
        .unwrap();

        let report = session_doctor_report(&session).unwrap();
        assert!(report.contains("last authorization diagnostic: timeout"));
        assert!(!report.contains("api_hash"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn doctor_report_uses_real_newlines_with_exact_line_structure() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-auth-doctor-newlines-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");

        let report = session_doctor_report(&session).unwrap();
        let lines: Vec<&str> = report.split('\n').collect();
        assert_eq!(
            lines.len(),
            7,
            "doctor report must be exactly 7 newline-separated lines"
        );
        assert_eq!(lines[0], format!("session path: {}", session.display()));
        assert_eq!(lines[1], "lock state: unlocked");
        assert_eq!(
            lines[2],
            "session file: type=absent, mode=not-applicable, readable=not-applicable, owner=not-applicable"
        );
        assert_eq!(
            lines[3],
            "session sidecars: -journal=absent, -wal=absent, -shm=absent"
        );
        assert_eq!(lines[4], "last authorization diagnostic: absent");
        assert_eq!(lines[5], "reauthorization required: false");
        let execution = if std::env::var_os("LAVIS_SERVICE").is_some() {
            "service"
        } else {
            "interactive"
        };
        assert_eq!(lines[6], format!("execution context: {execution}"));
        assert!(
            !report.contains("\\n"),
            "literal backslash-n must never appear in doctor output"
        );
        assert!(
            !report.ends_with('\n'),
            "doctor report must not end with a trailing newline"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reset_requires_an_interactive_terminal() {
        assert!(matches!(
            require_interactive_session_reset(false, true),
            Err(ClientError::SessionResetNonInteractive)
        ));
        assert!(matches!(
            require_interactive_session_reset(true, false),
            Err(ClientError::SessionResetNonInteractive)
        ));
        assert!(require_interactive_session_reset(true, true).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn reset_moves_only_session_files_to_a_private_backup() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-auth-reset-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        for suffix in ["", "-journal", "-wal", "-shm"] {
            fs::write(format!("{}{}", session.display(), suffix), "session data").unwrap();
        }
        let unrelated = directory.join("settings.json");
        fs::write(&unrelated, "persistent").unwrap();

        let backup = reset_session_with_backup(&session).unwrap().unwrap();
        assert_eq!(
            fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for suffix in ["", "-journal", "-wal", "-shm"] {
            assert!(!std::path::PathBuf::from(format!("{}{}", session.display(), suffix)).exists());
            assert_eq!(
                fs::read_to_string(backup.join(format!("session{suffix}"))).unwrap(),
                "session data"
            );
        }
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "persistent");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reset_recovers_an_interrupted_staged_backup_before_retrying() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-auth-reset-recovery-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        let staging = directory.join(".session-backup-interrupted.staging");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("session"), "session data").unwrap();

        let backup = reset_session_with_backup(&session).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(backup.join("session")).unwrap(),
            "session data"
        );
        assert!(!staging.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn staged_recovery_preflights_all_targets_before_any_mutation() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-auth-reset-preflight-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        // The staging directory holds several session sidecars, but one target
        // already exists in the parent. Recovery must detect the conflict
        // during preflight and move nothing: no partial recovery.
        let staging = directory.join(".session-backup-interrupted.staging");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("session"), "staged session").unwrap();
        fs::write(staging.join("session-wal"), "staged wal").unwrap();
        fs::write(staging.join("session-shm"), "staged shm").unwrap();
        fs::write(directory.join("session-wal"), "live wal").unwrap();

        assert!(matches!(
            reset_session_with_backup(&session),
            Err(ClientError::BackupSessionFile)
        ));
        assert!(!directory.join("session").exists(), "session was moved");
        assert_eq!(
            fs::read_to_string(directory.join("session-wal")).unwrap(),
            "live wal",
            "conflicting target was overwritten"
        );
        assert!(!directory.join("session-shm").exists(), "shm was moved");
        assert!(staging.join("session").exists(), "staged session was moved");
        assert!(staging.join("session-shm").exists(), "staged shm was moved");
        fs::remove_dir_all(directory).unwrap();
    }

    fn write_session_files(session: &std::path::Path) {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            fs::write(
                format!("{}{}", session.display(), suffix),
                format!("text{suffix}"),
            )
            .unwrap();
        }
    }

    fn assert_live_session_and_no_backups(directory: &std::path::Path, session: &std::path::Path) {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let path = std::path::PathBuf::from(format!("{}{}", session.display(), suffix));
            assert!(
                path.exists(),
                "live session file {suffix:?} must be restored"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), format!("text{suffix}"));
        }
        let has_backup = fs::read_dir(directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".session-backup-")
            });
        assert!(
            !has_backup,
            "no staging or published backup may remain after rollback"
        );
    }

    #[test]
    fn reset_rolls_back_a_sync_failure_after_the_first_move() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-reset-fault-first-move-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        write_session_files(&session);
        test_hooks::fail_next_sync_in(&directory, test_hooks::SyncScope::Parent, 1);

        assert!(matches!(
            reset_session_with_backup(&session),
            Err(ClientError::BackupSessionFile)
        ));
        assert_live_session_and_no_backups(&directory, &session);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reset_rolls_back_a_failure_of_the_staging_directory_sync() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-reset-fault-staging-sync-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        write_session_files(&session);
        test_hooks::fail_next_sync_in(&directory, test_hooks::SyncScope::Backup, 1);

        assert!(matches!(
            reset_session_with_backup(&session),
            Err(ClientError::BackupSessionFile)
        ));
        assert_live_session_and_no_backups(&directory, &session);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reset_rolls_back_a_durability_failure_before_publish() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-reset-fault-before-publish-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        write_session_files(&session);
        // The fifth parent sync is the final pre-publish durability sync.
        test_hooks::fail_next_sync_in(&directory, test_hooks::SyncScope::Parent, 5);

        assert!(matches!(
            reset_session_with_backup(&session),
            Err(ClientError::BackupSessionFile)
        ));
        assert_live_session_and_no_backups(&directory, &session);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reset_still_succeeds_when_only_the_post_publish_durability_sync_fails() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-reset-fault-post-publish-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        write_session_files(&session);
        // The sixth parent sync is the post-commit durability sync; it must not
        // turn an already-completed reset into a reported failure.
        test_hooks::fail_next_sync_in(&directory, test_hooks::SyncScope::Parent, 6);

        let backup = reset_session_with_backup(&session)
            .unwrap()
            .expect("reset must succeed when only the final durability sync fails");
        for suffix in ["", "-journal", "-wal", "-shm"] {
            assert!(backup.join(format!("session{suffix}")).exists());
            assert!(
                !std::path::PathBuf::from(format!("{}{}", session.display(), suffix)).exists(),
                "live session file {suffix:?} must stay moved into the backup"
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn commit_publish_conflict_rolls_back_to_the_live_session() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-reset-fault-publish-conflict-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        fs::write(&session, "session data").unwrap();
        let staging = directory.join(".session-backup-known.staging");
        fs::create_dir(&staging).unwrap();
        let mut transaction = SessionBackupTransaction::begin(staging.clone());
        transaction
            .move_live_file(session.clone(), staging.join("session"), &directory)
            .unwrap();
        // A non-empty directory at the published path forces the commit rename
        // to fail; rollback must restore the live session untouched. The
        // published name drops the leading dot and `.staging` suffix.
        let published = directory.join("session-backup-known");
        fs::create_dir(&published).unwrap();
        fs::write(published.join("occupant"), "conflict").unwrap();

        assert!(transaction.commit(&directory).is_err());
        transaction.rollback();
        assert_eq!(fs::read_to_string(&session).unwrap(), "session data");
        assert!(!staging.exists(), "empty staging directory must be removed");
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn staged_recovery_rejects_a_symlinked_staging_directory() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-reset-recovery-staging-symlink-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        fs::write(&session, "live session data").unwrap();
        let outside = directory.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("session"), "staged data").unwrap();
        symlink(&outside, directory.join(".session-backup-evil.staging")).unwrap();

        assert!(matches!(
            reset_session_with_backup(&session),
            Err(ClientError::BackupSessionFile)
        ));
        assert_eq!(
            fs::read_to_string(outside.join("session")).unwrap(),
            "staged data"
        );
        assert_eq!(fs::read_to_string(&session).unwrap(), "live session data");
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn staged_recovery_rejects_a_symlinked_child_session() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-reset-recovery-child-symlink-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        fs::write(&session, "live session data").unwrap();
        fs::write(directory.join("session-wal"), "live wal data").unwrap();
        let staging = directory.join(".session-backup-child.staging");
        fs::create_dir(&staging).unwrap();
        let outside = directory.join("outside-session");
        fs::write(&outside, "outside data").unwrap();
        symlink(&outside, staging.join("session")).unwrap();
        fs::write(staging.join("session-wal"), "staged wal data").unwrap();

        assert!(matches!(
            reset_session_with_backup(&session),
            Err(ClientError::BackupSessionFile)
        ));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "outside data");
        assert_eq!(fs::read_to_string(&session).unwrap(), "live session data");
        assert_eq!(
            fs::read_to_string(directory.join("session-wal")).unwrap(),
            "live wal data"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn reset_refuses_a_contended_session_without_mutation() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-auth-reset-lock-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        fs::write(&session, "session data").unwrap();
        let lock = crate::session::SessionLock::acquire(
            &session,
            crate::session::SessionLockContext::Client,
        )
        .unwrap();

        assert!(matches!(
            reset_session_with_backup(&session),
            Err(ClientError::SessionLocked)
        ));
        assert_eq!(fs::read_to_string(&session).unwrap(), "session data");
        assert!(fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("session-backup-")
        }));
        drop(lock);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn staging_root_is_private_and_rejects_symlinks_and_insecure_directories() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "lavis-staging-root-{}-{seq}",
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
    fn authorization_failure_explains_auth_key_duplication_recovery() {
        let error = authorization_failure(
            anyhow::Error::new(crate::error::AuthError::AuthorizationCheck(
                AuthorizationCheckFailure::AuthKeyDuplicated {
                    code: 500,
                    symbolic_name: "AUTH_KEY_DUPLICATED".to_owned(),
                },
            )),
            false,
        );

        assert!(error.to_string().contains("lavis auth reset --backup"));
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("category: auth_key_duplicated"))
        );
    }

    #[test]
    fn requires_manual_recovery_classifies_terminal_session_failures() {
        let duplicated = anyhow::Error::new(crate::error::AuthError::AuthorizationCheck(
            AuthorizationCheckFailure::AuthKeyDuplicated {
                code: 500,
                symbolic_name: "AUTH_KEY_DUPLICATED".to_owned(),
            },
        ));
        assert!(requires_manual_recovery(&duplicated));

        let malformed = anyhow::Error::new(ClientError::MalformedSession)
            .context("failed to open the Telegram session");
        assert!(requires_manual_recovery(&malformed));

        let transient = anyhow::Error::new(crate::error::AuthError::AuthorizationCheck(
            AuthorizationCheckFailure::Rpc {
                code: 500,
                symbolic_name: "INTERNAL".to_owned(),
            },
        ));
        assert!(!requires_manual_recovery(&transient));

        let timeout = anyhow::Error::new(crate::error::AuthError::AuthorizationCheck(
            AuthorizationCheckFailure::Timeout,
        ));
        assert!(!requires_manual_recovery(&timeout));

        let transport = anyhow::Error::new(crate::error::AuthError::AuthorizationCheck(
            AuthorizationCheckFailure::Transport,
        ));
        assert!(!requires_manual_recovery(&transport));

        let unrelated = anyhow::Error::new(ClientError::RunnerTask);
        assert!(!requires_manual_recovery(&unrelated));
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
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "lavis-logout-{}-{seq}",
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
            identity: crate::auth::SelfIdentity {
                username: None,
                display_name: None,
                id: PeerId::self_user(),
            },
        };
        assert!(should_show_quick_start(&outcome));
    }

    #[test]
    fn should_show_quick_start_is_false_for_existing_session() {
        use grammers_session::types::PeerId;
        let outcome = AuthorizationOutcome::ExistingSession {
            identity: crate::auth::SelfIdentity {
                username: None,
                display_name: None,
                id: PeerId::self_user(),
            },
        };
        assert!(!should_show_quick_start(&outcome));
    }

    #[test]
    fn render_quick_start_uses_non_default_prefix() {
        let text = render_quick_start("🦀", Some(crate::i18n::Locale::Russian));
        assert!(text.contains("🦀start"));
        assert!(!text.contains(",help"));
    }

    #[test]
    fn render_quick_start_preserves_russian_and_emoji() {
        let text = render_quick_start(",", Some(crate::i18n::Locale::Russian));
        assert!(text.contains("Авторизация завершена"));
        assert!(text.contains(",start"));
    }

    #[test]
    fn render_quick_start_contains_no_sensitive_data() {
        let text = render_quick_start(",", Some(crate::i18n::Locale::Russian));
        assert!(!text.contains("/home/"));
        assert!(!text.contains("api_id"));
        assert!(!text.contains("api_hash"));
        assert!(!text.contains("session"));
        assert!(!text.contains("credentials"));
    }

    #[test]
    fn render_quick_start_fallback_is_russian_and_includes_text() {
        let inner = render_quick_start(",", Some(crate::i18n::Locale::Russian));
        let fallback = render_quick_start_fallback(&inner, Some(crate::i18n::Locale::Russian));
        assert!(fallback.starts_with("Не удалось отправить приглашение в Telegram."));
        assert!(fallback.contains(inner.as_str()));
        assert!(fallback.contains("Авторизация завершена"));
    }

    #[test]
    fn quick_start_fallback_separates_fallback_and_invite_with_exactly_two_newlines() {
        let inner = render_quick_start(",", Some(crate::i18n::Locale::Russian));
        let fallback = render_quick_start_fallback(&inner, Some(crate::i18n::Locale::Russian));
        let expected = format!("Не удалось отправить приглашение в Telegram.\n\n{inner}");
        assert_eq!(fallback, expected);
        assert!(
            !fallback.contains("\\n"),
            "literal backslash-n must never appear in quick start output"
        );
        assert!(
            !fallback.ends_with('\n'),
            "quick start fallback must not end with a trailing newline"
        );
    }

    #[test]
    fn quick_start_fallback_bilingual_uses_two_newlines() {
        let inner = render_quick_start("!", None);
        let fallback = render_quick_start_fallback(&inner, None);
        let expected = format!(
            "{}\n\n{inner}",
            crate::i18n::bilingual(crate::i18n::Text::PostAuthFallback, "")
        );
        assert_eq!(fallback, expected);
    }
}
