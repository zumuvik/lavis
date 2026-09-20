use std::{
    collections::{VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

use futures_util::future::join_all;
use grammers_client::{Client, message::Message, tl};
use grammers_session::types::PeerId;

use crate::{
    aliases::{Alias, AliasStore, DeleteResult},
    auth::SelfIdentity,
    bot_api::{BotApi, HttpBotApi},
    command::Command,
    commands::{
        Action, AliasRequest, ExternalInvocation, LanguageRequest, LmRequest, ModulesRequest,
        PrefixRequest, SetupRequest, StartRequest, dispatch,
    },
    error::ExternalError,
    external_modules::{
        acquisition::{AcquisitionLimits, ModuleSourceAcquirer},
        approval::{
            ApprovalError, ApprovalId, ApprovalLimits, ApprovalStore, DEFAULT_APPROVAL_TTL,
        },
        events::{
            EventScope, module_can_receive_event, opaque_message_ref, validate_reaction_action,
        },
        manifest::ExternalCapability,
        protocol::{EventAction, MessageEvent, MessageEventKind},
        source_inspection::{
            InspectionConfig, InspectionLimits, ModuleInspector, OsRandom, SystemClock,
        },
    },
    external_modules::{
        control,
        manager::{ExternalManagerHandle, ExternalModuleRuntimeStatus, ExternalRuntimeSnapshot},
        state::ExternalStateStore,
    },
    fastfetch::{self, FastfetchInputError, FastfetchResult},
    help::{
        render_modules_invalid_usage, render_modules_overview_with_external_locale,
        render_with_external_locale,
    },
    i18n::{
        AliasText, ExternalCommandText, FastfetchText, InfoCaptionData, InfoText, LmInfoResponse,
        LmInstallPlanText, LmLabel, LmText, Locale, PingText, PrefixText, RuntimeText,
        SensitiveText, SetupText, StatsText, Text, alias_text, external_command_text,
        fastfetch_text, info_text, inspection_warning_text, lm_format, lm_label, lm_runtime_status,
        lm_state_text, lm_text, ping_text, prefix_text, render_info_text,
        render_lm_doctor_missing_catalog, render_lm_doctor_module, render_lm_doctor_report,
        render_lm_info, render_lm_install_plan, render_revision_status, render_stats_text,
        runtime_text, sensitive_text, setup_text, stats_text, text,
    },
    onboarding::OnboardingProgress,
    response::{Response, sanitize_external_output},
    settings::{DEFAULT_PREFIX, SettingsStore},
    setup::UsernameCandidate,
    setup_store::SetupStore,
    setup_telegram::{BotFatherProgress, CompanionSetup, GrammersTelegramSetup, ProvisionRequest},
    upstream::{
        RevisionRelation, UpstreamRev, UpstreamRevision, VersionRelation, version_relation,
    },
};

mod dispatch;

mod external;

mod info;

mod install;

#[cfg(test)]
use install::render_install_plan;

#[cfg(test)]
use info::{ProcStats, fastfetch_response, format_duration, parse_memory_kib, parse_system_uptime};
use info::{
    bounded_list, capabilities_label, commands_label, diagnostic_label, enabled_label,
    format_latency, format_stats, fresh_runtime_status, log_ping_failure,
    log_unavailable_proc_stats, management_label, read_proc_stats, runtime_status_from_snapshot,
    telegram_ping,
};

mod lm;

mod policy;

mod setup;

use setup::{SetupCoordinator, SetupPhase};
#[cfg(test)]
use setup::{setup_status_label, setup_status_response};

use policy::SensitiveCommandPolicy;
#[cfg(test)]
use policy::{SensitiveCommandDenial, authorize_sensitive_message};

#[cfg(test)]
use lm::lm_usage;

pub use dispatch::CreatedEventDispatchResult;

/// Setup-store context for the reactions veto and audit trail: resolved once
/// per message-event batch, before the reaction task is spawned.
pub(crate) struct ReactionAuditContext {
    pub(crate) state_path: std::path::PathBuf,
    pub(crate) token_path: std::path::PathBuf,
    pub(crate) companion_chat_id: Option<i64>,
}
#[cfg(test)]
pub(crate) use dispatch::external_event_error_category;

pub struct RuntimeState {
    started_at: Instant,
    recognized_commands: u64,
    aliases: AliasStore,
    settings: SettingsStore,
    self_identity: Option<SelfIdentity>,
    upstream: Option<Box<dyn UpstreamRev>>,
    upstream_revision_cache: UpstreamSnapshot,
    upstream_version_known: bool,
    info_local_metadata: InfoLocalMetadata,
    external_manager: Option<ExternalManagerHandle>,
    external_snapshot: ExternalRuntimeSnapshot,
    expected_self_edits: crate::message_provenance::SharedSelfEditLedger,
    setup_notification_ids: VecDeque<(PeerId, i32)>,
    bot_form_ledger: crate::message_provenance::SharedBotFormLedger,
    setup_edit_fallback_sources: VecDeque<(PeerId, i32)>,
    setup: Option<SetupCoordinator>,
    // Projection is held closed after setup is configured until BotFather's
    // authoritative peer identity has been resolved for this process.
    external_projection_permitted: bool,
    module_installation: Option<ModuleInstallation>,
    module_control: Option<ModuleControlConfig>,
    module_approvals: ApprovalStore<SystemClock, OsRandom>,
    external_warnings_announced: std::collections::HashSet<String>,
}

struct ModuleInstallation {
    root: PathBuf,
    staging_root: PathBuf,
    saved_messages_peer: PeerId,
}

struct ModuleControlConfig {
    root: PathBuf,
    state_path: PathBuf,
    declarative_state_path: PathBuf,
    saved_messages_peer: PeerId,
}

const MODULE_APPROVAL_LIMIT: usize = 8;
const MODULE_APPROVAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_EXPECTED_SELF_EDITS: usize = 128;
const SETUP_STAGE_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone)]
pub(crate) enum UpstreamSnapshot {
    Never,
    Success(UpstreamRevision),
    LastFailure {
        stale: Option<UpstreamRevision>,
        category: String,
    },
}

struct InfoLocalMetadata {
    host: &'static str,
    os: String,
    media: Option<String>,
}

#[derive(Debug)]
pub(crate) enum UpstreamResolveFailure {
    /// The `info/refs` lookup for upstream `main` itself failed.
    MainRev(crate::upstream::UpstreamError),
    /// Tangled answered with a rate limit.
    RateLimited { retry_after: Option<Duration> },
}

/// Resolves the upstream `main` revision together with the current build's
/// relation to it: one `info/refs` request plus at most two ordered compares
/// (`compare(main, current)` first, then the reverse only when the merge base
/// does not already prove an Ahead relation).
///
/// Deliberately free of caching and deadlines. The refresh worker owns the
/// overall deadline, while this function remains directly testable for the
/// network shape.
pub(crate) async fn resolve_upstream_revision(
    upstream: &dyn UpstreamRev,
) -> Result<UpstreamRevision, UpstreamResolveFailure> {
    resolve_upstream_relation_for(upstream, crate::info::build_rev()).await
}

async fn resolve_upstream_relation_for(
    upstream: &dyn UpstreamRev,
    current_rev: &str,
) -> Result<UpstreamRevision, UpstreamResolveFailure> {
    let main_rev = upstream.main_rev().await.map_err(|error| match error {
        crate::upstream::UpstreamError::RateLimited { retry_after } => {
            UpstreamResolveFailure::RateLimited { retry_after }
        }
        error => UpstreamResolveFailure::MainRev(error),
    })?;
    let relation = if current_rev == "unknown" {
        RevisionRelation::Unavailable
    } else if current_rev == main_rev {
        RevisionRelation::Current
    } else {
        let first_compare = match upstream.compare(&main_rev, current_rev).await {
            Ok(first_compare) => first_compare,
            Err(crate::upstream::UpstreamError::RevisionNotFound { .. }) => {
                // Current commit not published on Tangled
                return Ok(UpstreamRevision {
                    revision: main_rev,
                    relation: RevisionRelation::Unavailable,
                    version: None,
                });
            }
            Err(crate::upstream::UpstreamError::RateLimited { retry_after }) => {
                return Err(UpstreamResolveFailure::RateLimited { retry_after });
            }
            Err(error) => {
                tracing::warn!(
                    event = "upstream_compare_failed",
                    category = %error,
                    base = %main_rev,
                    head = %current_rev,
                    "Could not compare current build with upstream main"
                );
                return Err(UpstreamResolveFailure::MainRev(error));
            }
        };
        match first_compare.merge_base.as_deref() {
            Some(base) if base == main_rev => {
                // merge_base == main: current is ahead of main; the reverse
                // question is already answered by construction.
                RevisionRelation::Ahead {
                    commits: first_compare.ahead_by,
                }
            }
            _ => match upstream.compare(current_rev, &main_rev).await {
                Ok(reverse_compare) => crate::upstream::relation_from_ordered_compares(
                    &main_rev,
                    current_rev,
                    first_compare,
                    reverse_compare,
                ),
                Err(crate::upstream::UpstreamError::RateLimited { retry_after }) => {
                    return Err(UpstreamResolveFailure::RateLimited { retry_after });
                }
                Err(error) => {
                    tracing::warn!(
                        event = "upstream_compare_failed",
                        category = %error,
                        base = %current_rev,
                        head = %main_rev,
                        "Reverse compare failed"
                    );
                    return Err(UpstreamResolveFailure::MainRev(error));
                }
            },
        }
    };
    Ok(UpstreamRevision {
        revision: main_rev,
        relation,
        version: None,
    })
}

pub(crate) enum SetupInput {
    Ignored,
    Consumed {
        response: Option<Response>,
        provision: Option<ProvisionRequest>,
    },
}

pub(crate) struct RuntimeExecution {
    pub response: Response,
    pub provision: Option<ProvisionRequest>,
    pub shutdown: Option<ShutdownReason>,
    pub post_edit: Option<PostEditAction>,
    pub onboarding_page: bool,
    /// Static media to deliver alongside `response` instead of editing the
    /// command message. Only `info` sets this today; when present, the update
    /// layer sends the media and falls back to a text edit on failure.
    pub media: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostEditAction {
    ArmRebootReceipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    Exit,
    Restart,
}

#[derive(Clone)]
pub(crate) struct MessageExecutionContext<'a> {
    pub(crate) message: &'a Message,
    pub(crate) edited: bool,
    pub(crate) authored_by_self: bool,
    pub(crate) replied: Option<Message>,
}

impl From<Response> for RuntimeExecution {
    fn from(response: Response) -> Self {
        Self {
            response,
            provision: None,
            shutdown: None,
            post_edit: None,
            onboarding_page: false,
            media: None,
        }
    }
}

impl RuntimeState {
    pub fn new(started_at: Instant, aliases: AliasStore, settings: SettingsStore) -> Self {
        Self {
            started_at,
            recognized_commands: 0,
            aliases,
            settings,
            self_identity: None,
            upstream: None,
            upstream_revision_cache: UpstreamSnapshot::Never,
            upstream_version_known: false,
            info_local_metadata: InfoLocalMetadata {
                host: crate::info::deployment_label(std::env::var("LAVIS_HOST").ok().as_deref()),
                os: crate::info::read_os_release_pretty_name()
                    .unwrap_or_else(|| std::env::consts::OS.to_owned()),
                media: Some(crate::info::INFO_MEDIA_URL.to_owned()),
            },
            external_manager: None,
            external_snapshot: ExternalRuntimeSnapshot::new(),
            expected_self_edits: crate::message_provenance::SharedSelfEditLedger::default(),
            bot_form_ledger: crate::message_provenance::SharedBotFormLedger::default(),
            setup_notification_ids: VecDeque::new(),
            setup_edit_fallback_sources: VecDeque::new(),
            setup: None,
            external_projection_permitted: true,
            module_installation: None,
            module_control: None,
            module_approvals: ApprovalStore::new(
                SystemClock,
                OsRandom,
                DEFAULT_APPROVAL_TTL,
                ApprovalLimits {
                    max_entries: MODULE_APPROVAL_LIMIT,
                    max_pending_expanded_bytes: MODULE_APPROVAL_BYTES,
                },
            ),
            external_warnings_announced: std::collections::HashSet::new(),
        }
    }

    pub fn configure_setup(
        &mut self,
        state_path: PathBuf,
        token_path: PathBuf,
        saved_messages_peer: PeerId,
    ) {
        self.setup = Some(SetupCoordinator {
            state_path,
            token_path,
            saved_messages_peer,
            botfather_peer: None,
            phase: SetupPhase::Idle,
        });
        self.external_projection_permitted = false;
    }

    /// Marks a resolved BotFather peer as setup-private. Resolution is kept in
    /// the update layer because it needs Telegram APIs; this state only guards
    /// routing once a peer is known.
    pub fn set_setup_botfather_peer(&mut self, peer: PeerId) {
        if let Some(setup) = &mut self.setup {
            setup.botfather_peer = Some(peer);
            self.external_projection_permitted = true;
        }
    }

    #[cfg(test)]
    fn external_projection_permitted_for_tests(&self) -> bool {
        self.external_projection_permitted
    }

    pub fn setup_protects_message(&self, peer: PeerId, authored_by_self: bool) -> bool {
        self.setup.as_ref().is_some_and(|setup| {
            setup.botfather_peer == Some(peer)
                || (authored_by_self && peer == setup.saved_messages_peer && setup.is_active())
        })
    }

    /// The update loop owns this deadline, so expiry does not depend on an
    /// unrelated inbound message arriving.
    pub(crate) fn setup_timeout_deadline(&self) -> Option<Instant> {
        match &self.setup.as_ref()?.phase {
            SetupPhase::AwaitingUsername { deadline, .. }
            | SetupPhase::AwaitingConfirmation { deadline, .. }
            | SetupPhase::Running { deadline, .. } => Some(*deadline),
            SetupPhase::Idle
            | SetupPhase::EnablingInline { .. }
            | SetupPhase::SettingUserpic { .. } => None,
        }
    }

    pub(crate) fn handle_setup_timeout(&mut self) -> Option<Response> {
        let locale = self.locale();
        let setup = self.setup.as_mut()?;
        let expired = matches!(
            setup.phase,
            SetupPhase::AwaitingUsername { deadline, .. }
                | SetupPhase::AwaitingConfirmation { deadline, .. }
                | SetupPhase::Running { deadline, .. }
                if deadline <= Instant::now()
        );
        if !expired {
            return None;
        }
        setup.phase = SetupPhase::Idle;
        Some(Response::plain(setup_text(locale, SetupText::TimedOut)))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn handle_setup_input(
        &mut self,
        client: &Client,
        peer: PeerId,
        authored_by_self: bool,
        outgoing: bool,
        edited: bool,
        text: &str,
        buttons: &[crate::setup_telegram::BotFatherButton],
    ) -> SetupInput {
        let locale = self.locale();
        let Some(setup) = &mut self.setup else {
            return SetupInput::Ignored;
        };
        if setup.botfather_peer == Some(peer) {
            if authored_by_self || outgoing || edited {
                return SetupInput::Consumed {
                    response: None,
                    provision: None,
                };
            }
            let outcome = setup
                .handle_botfather_reply(client, text, buttons, locale)
                .await;
            let resolved = setup.botfather_peer.is_some();
            if resolved {
                self.external_projection_permitted = true;
            }
            return SetupInput::Consumed {
                response: outcome.response,
                provision: outcome.provision,
            };
        }
        if !authored_by_self || peer != setup.saved_messages_peer || !setup.is_active() {
            return SetupInput::Ignored;
        }
        SetupInput::Consumed {
            response: Some(setup.handle_input(client, text, locale).await),
            provision: None,
        }
    }

    pub async fn set_external_manager(&mut self, handle: ExternalManagerHandle) {
        self.external_snapshot = handle.snapshot().await;
        self.external_manager = Some(handle);
    }

    pub fn set_self_edit_ledger(
        &mut self,
        ledger: crate::message_provenance::SharedSelfEditLedger,
    ) {
        self.expected_self_edits = ledger;
    }

    pub fn set_bot_form_ledger(&mut self, ledger: crate::message_provenance::SharedBotFormLedger) {
        self.bot_form_ledger = ledger;
    }

    pub(crate) fn consume_bot_form_message(&mut self, peer_id: PeerId, message_id: i32) -> bool {
        self.bot_form_ledger.suppresses(peer_id, message_id)
    }

    pub fn configure_module_installation(
        &mut self,
        root: PathBuf,
        staging_root: PathBuf,
        self_user_id: PeerId,
    ) {
        self.module_installation = Some(ModuleInstallation {
            root,
            staging_root,
            saved_messages_peer: self_user_id,
        });
    }

    pub fn configure_module_control(
        &mut self,
        root: PathBuf,
        state_path: PathBuf,
        declarative_state_path: PathBuf,
        saved_messages_peer: PeerId,
    ) {
        self.module_control = Some(ModuleControlConfig {
            root,
            state_path,
            declarative_state_path,
            saved_messages_peer,
        });
    }

    pub fn shutdown_module_approvals(&mut self) {
        match self.module_approvals.shutdown() {
            Ok(removed) => tracing::debug!(
                event = "external_module_approvals_shutdown",
                removed,
                "Removed pending external module approvals"
            ),
            Err(error) => tracing::warn!(
                event = "external_module_approvals_shutdown_failed",
                error = %error,
                "Could not fully shut down external module approvals"
            ),
        }
    }

    pub fn external_manager(&self) -> Option<&ExternalManagerHandle> {
        self.external_manager.as_ref()
    }

    /// Stores the identity captured during authorization so `info` can render
    /// the owner without a per-invocation Telegram RPC.
    pub fn set_self_identity(&mut self, identity: SelfIdentity) {
        self.self_identity = Some(identity);
    }

    pub fn self_identity(&self) -> Option<&SelfIdentity> {
        self.self_identity.as_ref()
    }

    /// Injects the upstream revision resolver. Tests use a fake; production
    /// calls `set_http_upstream` once at startup.
    pub fn set_upstream(&mut self, upstream: Box<dyn UpstreamRev>) {
        self.upstream = Some(upstream);
    }

    pub fn set_http_upstream(&mut self) {
        match crate::upstream::HttpUpstreamRev::new() {
            Ok(client) => self.upstream = Some(Box::new(client)),
            Err(error) => tracing::warn!(
                event = "upstream_client_unavailable",
                ?error,
                "Upstream revision lookup will report unavailable"
            ),
        }
    }

    pub(crate) fn take_upstream(&mut self) -> Option<Box<dyn UpstreamRev>> {
        self.upstream.take()
    }

    pub(crate) fn publish_upstream_revision(&mut self, revision: Option<UpstreamRevision>) {
        if let Some(revision) = revision {
            if revision.version.is_some() {
                self.upstream_version_known = true;
            }
            let version = match &self.upstream_revision_cache {
                UpstreamSnapshot::Success(previous)
                | UpstreamSnapshot::LastFailure {
                    stale: Some(previous),
                    ..
                } => revision
                    .version
                    .clone()
                    .or_else(|| previous.version.clone()),
                _ => revision.version.clone(),
            };
            let mut revision = revision;
            revision.version = version;
            self.upstream_revision_cache = UpstreamSnapshot::Success(revision);
        }
    }

    pub(crate) fn publish_upstream_version(&mut self, version: Option<String>) {
        self.upstream_version_known = true;
        match &mut self.upstream_revision_cache {
            UpstreamSnapshot::Success(revision) => revision.version = version.clone(),
            UpstreamSnapshot::LastFailure { stale, .. } => {
                if let Some(revision) = stale {
                    revision.version = version;
                }
            }
            UpstreamSnapshot::Never => {}
        }
    }

    pub(crate) fn publish_upstream_failure(&mut self, category: impl Into<String>) {
        let stale = match &self.upstream_revision_cache {
            UpstreamSnapshot::Success(revision) => Some(revision.clone()),
            UpstreamSnapshot::LastFailure { stale, .. } => stale.clone(),
            UpstreamSnapshot::Never => None,
        };
        self.upstream_revision_cache = UpstreamSnapshot::LastFailure {
            stale,
            category: category.into(),
        };
    }

    fn upstream_revision(&self) -> Option<UpstreamRevision> {
        match &self.upstream_revision_cache {
            UpstreamSnapshot::Success(revision) => Some(revision.clone()),
            UpstreamSnapshot::LastFailure { stale, category } => {
                tracing::debug!(category = %category, "Serving upstream snapshot after refresh failure");
                stale.clone()
            }
            UpstreamSnapshot::Never => None,
        }
    }

    pub async fn refresh_snapshot(&mut self) {
        if let Some(handle) = &self.external_manager {
            self.external_snapshot = handle.snapshot().await;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_external_snapshot_for_tests(&mut self, snapshot: ExternalRuntimeSnapshot) {
        self.external_snapshot = snapshot;
    }

    pub fn prefix(&self) -> &str {
        self.settings.prefix()
    }

    pub(crate) fn locale(&self) -> Locale {
        self.settings.locale().unwrap_or(Locale::Russian)
    }

    /// Resolves the companion identity and setup-store paths for the
    /// reactions veto and audit trail. Without a setup coordinator there is
    /// no companion chat, so the audit is simply disabled.
    pub(crate) async fn reaction_audit_context(&self) -> Option<ReactionAuditContext> {
        match &self.setup {
            Some(coordinator) => coordinator.reaction_audit_context().await,
            None => None,
        }
    }

    pub fn register_expected_self_edit(
        &mut self,
        peer_id: PeerId,
        message_id: i32,
        text: String,
    ) -> Result<(), crate::message_provenance::LedgerFull> {
        self.expected_self_edits.register(peer_id, message_id, text)
    }

    pub fn consume_expected_self_edit(
        &mut self,
        peer_id: PeerId,
        message_id: i32,
        text: &str,
    ) -> bool {
        self.expected_self_edits.consume(peer_id, message_id, text)
    }

    pub fn remove_expected_self_edit(&mut self, peer_id: PeerId, message_id: i32, text: &str) {
        self.expected_self_edits.remove(peer_id, message_id, text);
    }

    pub fn register_setup_notification(&mut self, peer_id: PeerId, message_id: i32) {
        if self.setup_notification_ids.len() == MAX_EXPECTED_SELF_EDITS {
            self.setup_notification_ids.pop_front();
        }
        self.setup_notification_ids.push_back((peer_id, message_id));
    }

    pub fn consume_setup_notification(&mut self, peer_id: PeerId, message_id: i32) -> bool {
        let Some(index) = self
            .setup_notification_ids
            .iter()
            .position(|notification| *notification == (peer_id, message_id))
        else {
            return false;
        };
        self.setup_notification_ids.remove(index);
        true
    }

    pub fn claim_setup_edit_fallback(&mut self, peer_id: PeerId, message_id: i32) -> bool {
        if self
            .setup_edit_fallback_sources
            .iter()
            .any(|source| *source == (peer_id, message_id))
        {
            return false;
        }
        if self.setup_edit_fallback_sources.len() == MAX_EXPECTED_SELF_EDITS {
            self.setup_edit_fallback_sources.pop_front();
        }
        self.setup_edit_fallback_sources
            .push_back((peer_id, message_id));
        true
    }

    pub fn resolve_alias(&self, name: &str, args: &str) -> Option<Action> {
        let invocation_args = match shell_words::split(args) {
            Ok(arguments) => arguments,
            Err(_)
                if self
                    .aliases
                    .lookup(name)
                    .is_some_and(|alias| alias.target == "fastfetch") =>
            {
                return Some(Action::Fastfetch(args.to_owned()));
            }
            Err(_) => return None,
        };
        let invocation = self.aliases.invocation(name, &invocation_args).ok()??;
        dispatch(&Command {
            name: invocation.target,
            args: shell_words::join(invocation.args),
        })
    }

    async fn execute_language(&mut self, request: &LanguageRequest) -> Response {
        match request {
            LanguageRequest::Show => match self.settings.locale() {
                Some(locale) => Response::plain_with_locale(
                    self.locale(),
                    format!(
                        "🌐 {}: {}",
                        text(locale, Text::LanguageCurrent),
                        locale.code()
                    ),
                ),
                None => Response::plain_with_locale(
                    self.locale(),
                    crate::i18n::bilingual(Text::LanguageChoose, self.prefix()),
                ),
            },
            LanguageRequest::Set(locale) => match self.settings.set_locale(Some(*locale)).await {
                Ok(()) => Response::plain_with_locale(
                    self.locale(),
                    format!(
                        "🌐 {}: {}",
                        text(*locale, Text::LanguageChanged),
                        locale.code()
                    ),
                ),
                Err(_) => Response::plain_with_locale(
                    self.locale(),
                    text(*locale, Text::LanguageSaveFailed),
                ),
            },
            LanguageRequest::Invalid => Response::plain_with_locale(
                self.locale(),
                format!("⚠️ {}", text(self.locale(), Text::LanguageUsage)),
            ),
        }
    }

    async fn execute_start(
        &mut self,
        client: &Client,
        request: &StartRequest,
        peer: PeerId,
    ) -> RuntimeExecution {
        if self.settings.locale().is_none()
            && matches!(request, StartRequest::Bot | StartRequest::Skip)
        {
            return Response::plain_with_locale(
                self.locale(),
                crate::i18n::bilingual(Text::OnboardingSelect, self.prefix()),
            )
            .into();
        }
        let progress = match request {
            StartRequest::Bot => {
                return self.execute_setup(client, &SetupRequest::Start, peer).await;
            }
            StartRequest::Invalid => {
                return Response::plain_with_locale(
                    self.locale(),
                    format!(
                        "⚠️ {}",
                        text(self.locale(), Text::StartUsage).replace("{prefix}", self.prefix())
                    ),
                )
                .into();
            }
            StartRequest::Skip => {
                if self.settings.locale().is_none() {
                    return Response::plain_with_locale(
                        self.locale(),
                        crate::i18n::bilingual(Text::OnboardingSelect, self.prefix()),
                    )
                    .into();
                }
                let locale = self.locale();
                return match self
                    .settings
                    .set_onboarding(OnboardingProgress::skip())
                    .await
                {
                    Ok(()) => Response::plain_with_locale(
                        self.locale(),
                        OnboardingProgress::Skipped.message(locale, self.prefix()),
                    )
                    .into(),
                    Err(_) => Response::plain_with_locale(
                        self.locale(),
                        text(locale, Text::OnboardingSaveFailed),
                    )
                    .into(),
                };
            }
            StartRequest::Locale(locale) => {
                if self.settings.set_locale(Some(*locale)).await.is_err() {
                    return Response::plain_with_locale(
                        self.locale(),
                        text(*locale, Text::LanguageSaveFailed),
                    )
                    .into();
                }
                OnboardingProgress::restart()
            }
            StartRequest::Begin => {
                let Some(_) = self.settings.locale() else {
                    return Response::plain_with_locale(
                        self.locale(),
                        crate::i18n::bilingual(Text::OnboardingSelect, self.prefix()),
                    )
                    .into();
                };
                match self.settings.onboarding() {
                    OnboardingProgress::NotStarted
                    | OnboardingProgress::Complete
                    | OnboardingProgress::Skipped => OnboardingProgress::restart(),
                    progress => progress,
                }
            }
        };
        let locale = self.locale();
        let response = format!(
            "{}\n\n{}",
            progress.message(locale, self.prefix()),
            text(locale, Text::StartUsage).replace("{prefix}", self.prefix())
        );
        match self.settings.set_onboarding(progress).await {
            Ok(()) => RuntimeExecution {
                response: Response::plain_with_locale(self.locale(), response),
                provision: None,
                shutdown: None,
                post_edit: None,
                onboarding_page: true,
                media: None,
            },
            Err(_) => {
                Response::plain_with_locale(self.locale(), text(locale, Text::OnboardingSaveFailed))
                    .into()
            }
        }
    }

    pub(crate) async fn mark_onboarding_delivered(&mut self) {
        let next = self.settings.onboarding().mark_delivered();
        if let Err(error) = self.settings.set_onboarding(next).await {
            tracing::warn!(event = "onboarding_progress_save_failed", error = %error, "Could not advance delivered tutorial page");
        }
    }

    fn execute_reboot(&self, context: MessageExecutionContext<'_>) -> RuntimeExecution {
        match self.authorize_sensitive_command(SensitiveCommandPolicy::Reboot, context, None) {
            Ok(()) => RuntimeExecution {
                response: Response::plain_with_locale(
                    self.locale(),
                    runtime_text(self.locale(), RuntimeText::Rebooting),
                ),
                provision: None,
                shutdown: None,
                post_edit: Some(PostEditAction::ArmRebootReceipt),
                onboarding_page: false,
                media: None,
            },
            Err(response) => response.into(),
        }
    }

    fn execute_modules(&self, request: &ModulesRequest, prefix: &str) -> Response {
        match request {
            ModulesRequest::Overview => {
                tracing::info!(
                    event = "modules_overview",
                    module_count = crate::modules::modules().len(),
                    command_count = crate::commands::commands().len(),
                    "Rendered module overview"
                );
                let rendered = render_modules_overview_with_external_locale(
                    prefix,
                    self.external_descriptors(),
                    self.external_command_refs(),
                    self.locale(),
                );
                if rendered.entity_fallback {
                    tracing::warn!(
                        event = "modules_entity_fallback",
                        "Module formatting was unavailable"
                    );
                }
                rendered.response
            }
            ModulesRequest::Invalid => render_modules_invalid_usage(prefix, self.locale()).response,
        }
    }

    pub(crate) async fn execute_prefix(&mut self, request: &PrefixRequest) -> Response {
        let locale = self.locale();
        match request {
            PrefixRequest::Show => Response::plain_with_locale(
                self.locale(),
                prefix_text(locale, PrefixText::Current).replace("{prefix}", self.prefix()),
            ),
            PrefixRequest::Set(prefix) => match self.settings.set_prefix(prefix.clone()).await {
                Ok(()) => Response::plain_with_locale(
                    self.locale(),
                    prefix_text(locale, PrefixText::Changed).replace("{prefix}", self.prefix()),
                ),
                Err(_) => Response::plain_with_locale(
                    self.locale(),
                    prefix_text(locale, PrefixText::ChangeFailed),
                ),
            },
            PrefixRequest::Reset => match self.settings.set_prefix(DEFAULT_PREFIX.to_owned()).await
            {
                Ok(()) => Response::plain_with_locale(
                    self.locale(),
                    prefix_text(locale, PrefixText::Reset).replace("{prefix}", self.prefix()),
                ),
                Err(_) => Response::plain_with_locale(
                    self.locale(),
                    prefix_text(locale, PrefixText::ResetFailed),
                ),
            },
            PrefixRequest::Invalid => Response::plain_with_locale(
                self.locale(),
                prefix_text(locale, PrefixText::Usage).replace("{prefix}", self.prefix()),
            ),
        }
    }

    async fn execute_alias(&mut self, request: &AliasRequest, prefix: &str) -> Response {
        let locale = self.locale();
        match request {
            AliasRequest::List => {
                let aliases = self.aliases.aliases();
                if aliases.is_empty() {
                    return Response::plain_with_locale(
                        self.locale(),
                        alias_text(locale, AliasText::Empty),
                    );
                }
                let items = aliases
                    .iter()
                    .map(|(name, alias)| {
                        let args = if alias.args.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", shell_words::join(&alias.args))
                        };
                        alias_text(locale, AliasText::ListItem)
                            .replace("{prefix}", prefix)
                            .replace("{name}", name)
                            .replace("{target}", &alias.target)
                            .replace("{args}", &args)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Response::plain_with_locale(
                    self.locale(),
                    alias_text(locale, AliasText::List).replace("{items}", &items),
                )
            }
            AliasRequest::Add { name, target, args } => match self
                .aliases
                .add(
                    name,
                    Alias {
                        target: target.clone(),
                        args: args.clone(),
                    },
                )
                .await
            {
                Ok(_) => Response::plain_with_locale(
                    self.locale(),
                    alias_text(locale, AliasText::Added)
                        .replace("{prefix}", prefix)
                        .replace("{name}", name),
                ),
                Err(_) => Response::plain_with_locale(
                    self.locale(),
                    alias_text(locale, AliasText::AddFailed),
                ),
            },
            AliasRequest::Delete { name } => match self.aliases.delete(name).await {
                Ok(DeleteResult::Deleted) => Response::plain_with_locale(
                    self.locale(),
                    alias_text(locale, AliasText::Deleted)
                        .replace("{prefix}", prefix)
                        .replace("{name}", name),
                ),
                Ok(DeleteResult::NotFound) => Response::plain_with_locale(
                    self.locale(),
                    alias_text(locale, AliasText::NotFound).replace("{name}", name),
                ),
                Err(_) => Response::plain_with_locale(
                    self.locale(),
                    alias_text(locale, AliasText::DeleteFailed),
                ),
            },
            AliasRequest::Show { name } => {
                let normalized_name = name.to_ascii_lowercase();
                let Some(alias) = self.aliases.lookup(name) else {
                    return Response::plain_with_locale(
                        self.locale(),
                        alias_text(locale, AliasText::DoesNotExist)
                            .replace("{prefix}", prefix)
                            .replace("{name}", &normalized_name),
                    );
                };
                let args = if alias.args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", shell_words::join(&alias.args))
                };
                Response::collapsed_with_locale(
                    locale,
                    alias_text(locale, AliasText::ShowHeading)
                        .replace("{prefix}", prefix)
                        .replace("{name}", &normalized_name),
                    alias_text(locale, AliasText::Target)
                        .replace("{prefix}", prefix)
                        .replace("{target}", &alias.target)
                        .replace("{args}", &args),
                )
                .response
            }
            AliasRequest::Invalid => Response::plain_with_locale(
                locale,
                alias_text(locale, AliasText::Usage).replace("{prefix}", prefix),
            ),
        }
    }
}

fn missing_descriptor_response(locale: Locale, module_text: &str, module_id: &str) -> Response {
    Response::plain_with_locale(
        locale,
        format!(
            "{}\n\n{}",
            sanitize_external_output(module_text),
            external_command_text(
                locale,
                ExternalCommandText::MissingDescriptor,
                module_id,
                None
            )
        ),
    )
}

pub(crate) fn invocation_error_category(
    error: &grammers_mtsender::InvocationError,
) -> &'static str {
    match error {
        grammers_mtsender::InvocationError::Session(_) => "session",
        grammers_mtsender::InvocationError::Rpc(_) => "rpc",
        grammers_mtsender::InvocationError::Io(_) => "io",
        grammers_mtsender::InvocationError::Deserialize(_) => "deserialize",
        grammers_mtsender::InvocationError::Transport(_) => "transport",
        grammers_mtsender::InvocationError::Dropped => "dropped",
        grammers_mtsender::InvocationError::InvalidDc => "invalid_dc",
        grammers_mtsender::InvocationError::Authentication(_) => "authentication",
    }
}

#[cfg(test)]
mod tests;
