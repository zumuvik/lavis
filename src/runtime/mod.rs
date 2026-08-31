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
    fastfetch::{self, FastfetchInputError, FastfetchProfileError, FastfetchResult},
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
#[cfg(test)]
pub(crate) use dispatch::external_event_error_category;

pub struct RuntimeState {
    started_at: Instant,
    recognized_commands: u64,
    aliases: AliasStore,
    settings: SettingsStore,
    fastfetch_profile_path: PathBuf,
    self_identity: Option<SelfIdentity>,
    upstream: Option<Box<dyn UpstreamRev>>,
    upstream_revision_cache: UpstreamSnapshot,
    upstream_version_known: bool,
    info_local_metadata: InfoLocalMetadata,
    external_manager: Option<ExternalManagerHandle>,
    external_snapshot: ExternalRuntimeSnapshot,
    expected_self_edits: crate::message_provenance::SharedSelfEditLedger,
    setup_notification_ids: VecDeque<(PeerId, i32)>,
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
    pub fn new(
        started_at: Instant,
        aliases: AliasStore,
        settings: SettingsStore,
        fastfetch_profile_path: PathBuf,
    ) -> Self {
        Self {
            started_at,
            recognized_commands: 0,
            aliases,
            settings,
            fastfetch_profile_path,
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
            SetupPhase::Idle => None,
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

    pub(crate) async fn handle_setup_input(
        &mut self,
        client: &Client,
        peer: PeerId,
        authored_by_self: bool,
        outgoing: bool,
        edited: bool,
        text: &str,
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
            let outcome = setup.handle_botfather_reply(client, text, locale).await;
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
mod tests {
    use super::{
        ProcStats, SensitiveCommandDenial, SensitiveCommandPolicy, UpstreamResolveFailure,
        UpstreamSnapshot, authorize_sensitive_message, bounded_list, external_event_error_category,
        fastfetch_response, format_duration, format_latency, format_stats, lm_usage,
        missing_descriptor_response, parse_memory_kib, parse_system_uptime, render_install_plan,
        resolve_upstream_relation_for, setup_status_label, setup_status_response,
    };
    use crate::external_modules::manager::{
        ExternalModuleRuntimeStatus, ExternalModuleStatus, ExternalRuntimeSnapshot,
    };
    use crate::response::Response;
    use crate::{
        aliases::{Alias, AliasStore},
        bot_api::{BotApi, BotApiFuture, BotIdentity},
        commands::{Action, AliasRequest},
        external_modules::approval::{APPROVAL_ID_BYTES, ApprovalId},
        external_modules::source_inspection::{
            ArchiveDigest, ArchiveStatistics, InspectionTimes, InspectionWarning,
            ModuleInstallPlan, SourceIdentity, SourceKind,
        },
        fastfetch::{FastfetchInputError, FastfetchProfileError, FastfetchResult},
        i18n::{
            Locale, PingText, RuntimeText, SensitiveText, ping_text, runtime_text, sensitive_text,
        },
        setup_store::{CompanionToken, PersistedSetupState, SetupStore},
        upstream::{CompareFuture, UpstreamError, UpstreamRev, UpstreamRevFuture},
    };
    use grammers_session::types::PeerId;
    use std::{
        collections::HashMap,
        fs,
        path::PathBuf,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn install_plan_renders_bounded_v6_method_grants() {
        let plan = ModuleInstallPlan {
            source_kind: SourceKind::Archive,
            source_identity: SourceIdentity::Archive,
            module_id: "raw".to_owned(),
            module_version: "1".to_owned(),
            protocol_version: 6,
            contract_revision: Some(2),
            entrypoint: "run".to_owned(),
            default_command: None,
            archive_digest: ArchiveDigest::from_hex(&"0".repeat(64)).unwrap(),
            archive: ArchiveStatistics {
                archive_bytes: 1,
                file_count: 1,
                compressed_bytes: 1,
                expanded_bytes: 1,
            },
            warnings: vec![
                InspectionWarning::StoredOnlyArchive,
                InspectionWarning::TelegramRawNotSandboxed,
            ],
            times: InspectionTimes {
                inspected_unix_seconds: 0,
                expires_unix_seconds: 0,
            },
            capabilities: vec!["telegram.raw".to_owned()],
            subscriptions: vec![],
            telegram_methods: vec!["account.updateStatus".to_owned()],
            actions: vec![],
            fingerprint: "fingerprint".to_owned(),
        };
        let approval_id = ApprovalId::from_bytes([0; APPROVAL_ID_BYTES]);
        for locale in [Locale::English, Locale::Russian] {
            let rendered = render_install_plan(locale, &plan, approval_id, ".");
            assert!(rendered.contains("account.updateStatus"));
            assert!(!rendered.contains("StoredOnlyArchive"));
            assert!(!rendered.contains("TelegramRawNotSandboxed"));
        }
    }

    #[test]
    fn setup_status_codes_are_localized_without_echoing_unknown_values() {
        for (locale, expected_unknown) in [
            (Locale::English, "unknown"),
            (Locale::Russian, "неизвестно"),
        ] {
            for status in [
                "idle",
                "bot_validated",
                "complete",
                "completed_without_folder_capacity",
                "completed_without_folder_name_conflict",
                "companion_and_community_configured",
                "companion_configured_community_pending",
            ] {
                assert_ne!(setup_status_label(locale, status), expected_unknown);
            }
            assert_eq!(
                setup_status_label(locale, "untrusted_internal_value"),
                expected_unknown
            );
        }
    }

    #[test]
    fn persisted_idle_status_has_exactly_one_heading_in_each_locale() {
        for (locale, expected) in [
            (
                Locale::English,
                "⚙️ Setup status: idle\nBot: not configured",
            ),
            (
                Locale::Russian,
                "⚙️ Состояние настройки: бездействует\nБот: не настроен",
            ),
        ] {
            assert_eq!(
                setup_status_response(locale, "idle", None),
                Response::plain(expected)
            );
        }
    }

    #[test]
    fn missing_descriptor_output_is_sanitized_in_each_locale() {
        for locale in [Locale::English, Locale::Russian] {
            let response =
                missing_descriptor_response(locale, "safe\x1b[31m\x00bidi\u{202e}", "missing");
            assert!(response.text.contains("safebidi"));
            assert!(!response.text.contains('\x1b'));
            assert!(!response.text.contains('\x00'));
            assert!(!response.text.contains('\u{202e}'));
        }
    }

    #[test]
    fn sensitive_command_policies_distinguish_saved_messages_from_reboot_dialogs() {
        let saved = PeerId::user(1).unwrap();
        let private = PeerId::user(2).unwrap();
        let group = PeerId::chat(3).unwrap();
        let supergroup = PeerId::channel(4).unwrap();

        for peer in [saved, private, group, supergroup] {
            assert_eq!(
                authorize_sensitive_message(
                    SensitiveCommandPolicy::Reboot,
                    false,
                    true,
                    peer,
                    1,
                    None,
                ),
                Ok(())
            );
        }
        assert_eq!(
            authorize_sensitive_message(SensitiveCommandPolicy::Reboot, true, true, group, 1, None,),
            Err(SensitiveCommandDenial::Edited)
        );
        assert_eq!(
            SensitiveCommandDenial::Edited
                .response(Locale::Russian, SensitiveCommandPolicy::Reboot),
            sensitive_text(Locale::Russian, SensitiveText::RebootDenied)
        );

        let request = SensitiveCommandPolicy::ModuleMutation;
        assert_eq!(
            authorize_sensitive_message(request, false, true, private, 1, Some(saved)),
            Err(SensitiveCommandDenial::NotSavedMessages)
        );
        assert_eq!(
            authorize_sensitive_message(request, false, false, saved, 1, Some(saved)),
            Err(SensitiveCommandDenial::NotSelfAuthored)
        );
        assert_eq!(
            SensitiveCommandDenial::NotSavedMessages.response(Locale::Russian, request),
            sensitive_text(Locale::Russian, SensitiveText::ModuleMutationDenied)
        );
    }

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    async fn runtime_with_alias() -> (super::RuntimeState, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "lavis-runtime-show-{}-{nonce}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("aliases.json");
        let mut aliases = AliasStore::load(path).await.unwrap();
        aliases
            .add(
                "Mini",
                Alias {
                    target: "fastfetch".to_owned(),
                    args: vec!["--separator".to_owned(), " → ".to_owned()],
                },
            )
            .await
            .unwrap();
        let settings = crate::settings::SettingsStore::load(directory.join("settings.json"))
            .await
            .unwrap();
        (
            super::RuntimeState::new(
                Instant::now(),
                aliases,
                settings,
                directory.join("fastfetch.json"),
            ),
            directory,
        )
    }

    #[tokio::test]
    async fn setup_timeout_ends_flow_without_an_inbound_botfather_update() {
        let (mut runtime, directory) = runtime_with_alias().await;
        runtime
            .settings
            .set_locale(Some(crate::i18n::Locale::English))
            .await
            .unwrap();
        let saved_messages = PeerId::user(1).unwrap();
        let botfather = PeerId::user(2).unwrap();
        runtime.configure_setup(
            directory.join("state.json"),
            directory.join("token"),
            saved_messages,
        );
        runtime.set_setup_botfather_peer(botfather);
        runtime.setup.as_mut().unwrap().phase = super::SetupPhase::AwaitingUsername {
            automatic: false,
            deadline: Instant::now() + Duration::from_millis(1),
        };

        let deadline = runtime.setup_timeout_deadline().unwrap();
        tokio::time::sleep_until(deadline.into()).await;
        let response = runtime.handle_setup_timeout().unwrap();

        assert!(response.text.contains("timed out"));
        assert!(runtime.setup_timeout_deadline().is_none());
        assert!(runtime.setup_protects_message(botfather, false));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn restart_without_botfather_resolution_fails_closed_for_token_text() {
        let (mut runtime, directory) = runtime_with_alias().await;
        runtime
            .settings
            .set_locale(Some(crate::i18n::Locale::English))
            .await
            .unwrap();
        runtime.configure_setup(
            directory.join("state.json"),
            directory.join("token"),
            PeerId::user(1).unwrap(),
        );
        assert!(!runtime.external_projection_permitted_for_tests());
        assert!(
            runtime
                .prepare_message_event_dispatch(
                    PeerId::user(2).unwrap(),
                    1,
                    crate::external_modules::protocol::MessageEventKind::Created,
                    "123456:abcdefghijklmnopqrstUVWX",
                    false,
                    vec![],
                )
                .is_none()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn lm_doctor_reports_installed_modules_and_unknown_targets() {
        use std::os::unix::fs::PermissionsExt;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "lavis-runtime-doctor-{}-{nonce}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();

        let module_dir = directory.join("sample");
        fs::create_dir_all(&module_dir).unwrap();
        fs::write(
            module_dir.join("module.json"),
            br#"{"schema_version":6,"id":"sample","name":"Sample","version":"1","author":"A","entrypoint":"run","capabilities":[],"telegram_methods":[],"commands":[{"name":"go","summary_ru":"x","description_ru":"x","usage":"<value>"}]}"#,
        )
        .unwrap();
        let entrypoint = module_dir.join("run");
        fs::write(&entrypoint, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o700)).unwrap();

        let (mut runtime, state_directory) = runtime_with_alias().await;
        runtime.configure_module_control(
            directory.clone(),
            directory.join("state.json"),
            directory.join("declarative.json"),
            PeerId::user(1).unwrap(),
        );

        let doctor_all = runtime.lm_doctor(None).await;
        assert!(doctor_all.text.contains("🩺 Диагностика внешних модулей"));
        assert!(doctor_all.text.contains("sample"));
        assert!(doctor_all.text.contains("Runtime: не запущен"));
        assert!(!doctor_all.text.contains("Последний сбой"));

        let doctor_one = runtime.lm_doctor(Some("sample")).await;
        assert!(doctor_one.text.contains("🩺 Диагностика модуля sample"));
        assert!(doctor_one.text.contains("sample"));

        let doctor_missing = runtime.lm_doctor(Some("absent")).await;
        assert!(doctor_missing.text.contains("Модуль absent не найден."));

        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(state_directory).unwrap();
    }

    #[tokio::test]
    async fn lm_doctor_reports_missing_catalog_only_for_absent_ids() {
        use std::os::unix::fs::PermissionsExt;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "lavis-runtime-doctor-ghost-{}-{nonce}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();

        let module_dir = directory.join("installed");
        fs::create_dir_all(&module_dir).unwrap();
        fs::write(
            module_dir.join("module.json"),
            br#"{"schema_version":6,"id":"installed","name":"Installed","version":"1","author":"A","entrypoint":"run","capabilities":[],"telegram_methods":[],"commands":[{"name":"go","summary_ru":"x","description_ru":"x","usage":"<value>"}]}"#,
        )
        .unwrap();
        let entrypoint = module_dir.join("run");
        fs::write(&entrypoint, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            directory.join("state.json"),
            br#"{"version":1,"enabled":["installed","ghost"]}"#,
        )
        .unwrap();

        let (mut runtime, state_directory) = runtime_with_alias().await;
        runtime.configure_module_control(
            directory.clone(),
            directory.join("state.json"),
            directory.join("declarative.json"),
            PeerId::user(1).unwrap(),
        );

        let doctor_all = runtime.lm_doctor(None).await;
        assert!(doctor_all.text.contains("Installed"));
        assert_eq!(
            doctor_all.text.matches("каталог отсутствует").count(),
            1,
            "only the genuinely absent id may be reported: {}",
            doctor_all.text
        );
        assert!(doctor_all.text.contains("ghost"));

        let doctor_one = runtime.lm_doctor(Some("installed")).await;
        assert!(doctor_one.text.contains("Installed"));
        assert_eq!(doctor_one.text.matches("каталог отсутствует").count(), 0);

        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(state_directory).unwrap();
    }

    #[tokio::test]
    async fn lm_info_distinguishes_a_missing_module_directory() {
        let (mut runtime, state_directory) = runtime_with_alias().await;
        let module_root = state_directory.join("modules");
        fs::create_dir_all(&module_root).unwrap();
        runtime.configure_module_control(
            module_root.clone(),
            state_directory.join("module-state.json"),
            state_directory.join("declarative.json"),
            PeerId::user(1).unwrap(),
        );

        let response = runtime.lm_info("missing").await;

        assert_eq!(response.text, "⚠️ Модуль не установлен.");
        fs::remove_dir_all(state_directory).unwrap();
    }

    #[tokio::test]
    async fn lm_info_logs_and_doctor_empty_responses_follow_the_locale() {
        let (mut runtime, state_directory) = runtime_with_alias().await;
        let module_root = state_directory.join("modules");
        fs::create_dir_all(&module_root).unwrap();
        runtime.configure_module_control(
            module_root,
            state_directory.join("module-state.json"),
            state_directory.join("declarative.json"),
            PeerId::user(1).unwrap(),
        );
        runtime
            .set_external_manager(
                crate::external_modules::manager::ExternalManagerHandle::new(
                    crate::external_modules::manager::ExternalManager::new(),
                ),
            )
            .await;

        for (locale, info_missing, logs_empty, doctor_all, doctor_missing) in [
            (
                Locale::Russian,
                "⚠️ Модуль не установлен.",
                "ℹ️ Для модуля missing нет сохранённой runtime-ошибки.",
                "🩺 Диагностика внешних модулей\n\nВнешние модули не установлены.",
                "ℹ️ Модуль missing не найден.",
            ),
            (
                Locale::English,
                "⚠️ Module is not installed.",
                "ℹ️ Module missing has no retained runtime error.",
                "🩺 External module diagnostics\n\nNo external modules are installed.",
                "ℹ️ Module missing was not found.",
            ),
        ] {
            runtime.settings.set_locale(Some(locale)).await.unwrap();
            assert_eq!(
                runtime.lm_info("missing").await,
                Response::plain(info_missing)
            );
            assert_eq!(
                runtime.lm_logs("missing").await,
                Response::plain(logs_empty)
            );
            assert_eq!(runtime.lm_doctor(None).await, Response::plain(doctor_all));
            assert_eq!(
                runtime.lm_doctor(Some("missing")).await,
                Response::plain(doctor_missing)
            );
        }

        fs::remove_dir_all(state_directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lm_status_commands_use_a_fresh_snapshot_after_asynchronous_v6_crash() {
        use std::{os::unix::fs::PermissionsExt, sync::Arc};

        struct NoopExecutor;
        impl crate::external_modules::v6_executor::V6TelegramExecutor for NoopExecutor {
            fn execute<'a>(
                &'a self,
                _context: crate::external_modules::v6_executor::V6ExecutionContext,
                _method: crate::external_modules::v6_registry::V6Method,
                _params: Box<serde_json::value::RawValue>,
            ) -> crate::external_modules::v6_executor::V6ExecutorFuture<'a> {
                Box::pin(async {
                    Err(crate::external_modules::v6_executor::V6ExecutorError::Transport)
                })
            }
        }

        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "lavis-runtime-fresh-status-{}-{nonce}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let module_dir = directory.join("sample");
        fs::create_dir_all(&module_dir).unwrap();
        let python = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join("python3"))
            .find(|candidate| candidate.is_file())
            .expect("fixture tests require python3 in PATH");
        let entrypoint = module_dir.join("run");
        let script = format!(
            "#!{}\nimport json, os, sys, time\nframe = json.loads(sys.stdin.readline())\nprint(json.dumps({{'protocol_version':6,'type':'initialized','request_id':frame['request_id'],'module_id':'sample'}}), flush=True)\nframe = json.loads(sys.stdin.readline())\nprint(json.dumps({{'protocol_version':6,'type':'health','request_id':frame['request_id']}}), flush=True)\ncrash_signal = os.path.join(os.path.dirname(__file__), 'crash')\nwhile not os.path.exists(crash_signal):\n    time.sleep(0.01)\nsys.exit(7)\n",
            python.display()
        );
        fs::write(&entrypoint, script).unwrap();
        fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            module_dir.join("module.json"),
            br#"{"schema_version":6,"id":"sample","name":"Sample","version":"1","author":"A","entrypoint":"run","capabilities":[],"telegram_methods":[],"commands":[{"name":"go","summary_ru":"x","description_ru":"x","usage":"<value>"}]}"#,
        )
        .unwrap();
        let descriptor = crate::external_modules::manifest::validate_manifest_at(
            &module_dir.join("module.json"),
            Some("sample"),
        )
        .unwrap();
        crate::external_modules::v6_process::ensure_test_state_base();
        let handle = crate::external_modules::manager::ExternalManagerHandle::new(
            crate::external_modules::manager::ExternalManager::new(),
        );
        {
            let mut manager = handle.lock().await;
            manager.set_descriptors(vec![descriptor]);
            manager.set_v6_executor(Arc::new(NoopExecutor));
        }
        handle
            .startup_enabled(&std::collections::BTreeSet::from(["sample".to_owned()]))
            .await;

        let (mut runtime, state_directory) = runtime_with_alias().await;
        runtime.configure_module_control(
            directory.clone(),
            directory.join("state.json"),
            directory.join("declarative.json"),
            PeerId::user(1).unwrap(),
        );
        runtime.set_external_manager(handle.clone()).await;
        assert_eq!(
            runtime.external_snapshot.module_statuses[0].status,
            crate::external_modules::manager::ExternalModuleRuntimeStatus::Running,
            "the cached routing snapshot intentionally predates the crash"
        );
        fs::write(module_dir.join("crash"), b"").unwrap();
        for _ in 0..200 {
            if handle
                .snapshot()
                .await
                .module_statuses
                .iter()
                .any(|status| {
                    status.id == "sample"
                        && status.status
                            == crate::external_modules::manager::ExternalModuleRuntimeStatus::Failed
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            handle
                .snapshot()
                .await
                .module_statuses
                .iter()
                .any(|status| {
                    status.id == "sample"
                        && status.status
                            == crate::external_modules::manager::ExternalModuleRuntimeStatus::Failed
                })
        );

        assert!(
            runtime
                .render_lm_list()
                .await
                .text
                .contains("Runtime: ошибка")
        );
        assert!(
            runtime
                .lm_info("sample")
                .await
                .text
                .contains("Runtime: ошибка")
        );
        assert!(
            runtime
                .lm_doctor(Some("sample"))
                .await
                .text
                .contains("Runtime: ошибка")
        );

        let russian_diagnostic = runtime.lm_logs("sample").await;
        assert!(
            russian_diagnostic
                .text
                .starts_with("📋 Последняя ошибка модуля sample\n\n")
        );
        assert!(russian_diagnostic.text.contains("stage="));

        runtime
            .settings
            .set_locale(Some(Locale::English))
            .await
            .unwrap();
        let english_diagnostic = runtime.lm_logs("sample").await;
        assert!(
            english_diagnostic
                .text
                .starts_with("📋 Last module error: sample\n\n")
        );
        assert!(english_diagnostic.text.contains("stage="));
        assert!(
            runtime
                .lm_info("sample")
                .await
                .text
                .contains("Runtime: failed")
        );
        assert!(
            runtime
                .lm_doctor(Some("sample"))
                .await
                .text
                .contains("Last failure:")
        );

        handle.shutdown_all().await;
        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(state_directory).unwrap();
    }

    #[tokio::test]
    async fn generated_username_has_no_botfather_side_effect_before_confirmation() {
        let mut setup = super::SetupCoordinator {
            state_path: PathBuf::new(),
            token_path: PathBuf::new(),
            saved_messages_peer: PeerId::user(1).unwrap(),
            botfather_peer: None,
            phase: super::SetupPhase::Idle,
        };

        let response = setup
            .confirm_or_start(
                crate::setup::generate_candidate().unwrap(),
                true,
                1,
                Locale::Russian,
            )
            .await;

        assert!(response.text.contains("confirm"));
        assert!(matches!(
            setup.phase,
            super::SetupPhase::AwaitingConfirmation { .. }
        ));
        assert!(setup.botfather_peer.is_none());
    }

    #[tokio::test]
    async fn interactive_username_transitions_to_confirmation_while_flow_is_active() {
        let mut setup = super::SetupCoordinator {
            state_path: PathBuf::new(),
            token_path: PathBuf::new(),
            saved_messages_peer: PeerId::user(1).unwrap(),
            botfather_peer: None,
            phase: super::SetupPhase::AwaitingUsername {
                automatic: false,
                deadline: Instant::now(),
            },
        };

        let response = setup
            .handle_username_input("lavis_test_bot", Locale::Russian)
            .await;

        assert_eq!(
            response.text,
            "📋 План настройки\n\n• Создать companion-бота @lavis_test_bot с именем «Lavis — really your userbot».\n• Создать или восстановить приватный Lavis workspace.\n• Присоединить ваш Telegram-аккаунт к официальному публичному сообществу @lavis_userbot.\n• Добавить workspace, бота и сообщество в папку Lavis.\n\nНапишите confirm для подтверждения или cancel для отмены."
        );
        assert!(matches!(
            setup.phase,
            super::SetupPhase::AwaitingConfirmation { .. }
        ));
    }

    #[tokio::test]
    async fn pending_setup_renders_with_the_locale_selected_at_response_time() {
        let mut setup = super::SetupCoordinator {
            state_path: PathBuf::new(),
            token_path: PathBuf::new(),
            saved_messages_peer: PeerId::user(1).unwrap(),
            botfather_peer: None,
            phase: super::SetupPhase::AwaitingUsername {
                automatic: false,
                deadline: Instant::now(),
            },
        };

        let response = setup
            .handle_username_input("invalid", Locale::English)
            .await;

        assert_eq!(
            response,
            Response::plain(
                "⚠️ The username must contain 5–32 ASCII letters, digits, or _ and end in _bot."
            )
        );
    }

    struct RepairBotApi {
        identity: Result<BotIdentity, crate::bot_api::BotApiError>,
    }

    impl BotApi for RepairBotApi {
        fn get_me<'a>(&'a self, _: &'a CompanionToken) -> BotApiFuture<'a> {
            Box::pin(async { self.identity.clone() })
        }
    }

    #[tokio::test]
    async fn repair_requires_a_verified_persisted_bot_identity() {
        let (mut runtime, directory) = runtime_with_alias().await;
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let state_path = directory.join("setup.json");
        let token_path = directory.join("token");
        let mut state = PersistedSetupState::default();
        state.stages.bot_created = true;
        state.identities.bot_username = Some("lavis_test_bot".to_owned());
        state.identities.bot_user_id = Some(7);
        let mut store = SetupStore::new(state_path.clone(), token_path.clone());
        store.save_state(&state).unwrap();
        store
            .save_token(&CompanionToken::new("123456:abcdefghijklmnopqrstUVWX".to_owned()).unwrap())
            .unwrap();
        runtime.configure_setup(state_path, token_path, PeerId::user(1).unwrap());
        let setup = runtime.setup.as_ref().unwrap();

        let wrong = RepairBotApi {
            identity: Ok(BotIdentity {
                id: 8,
                username: "lavis_test_bot".to_owned(),
            }),
        };
        assert!(
            setup
                .repair_preflight(&wrong, Locale::Russian)
                .await
                .is_err()
        );

        let matching = RepairBotApi {
            identity: Ok(BotIdentity {
                id: 7,
                username: "LAVIS_TEST_BOT".to_owned(),
            }),
        };
        assert_eq!(
            setup
                .repair_preflight(&matching, Locale::Russian)
                .await
                .unwrap(),
            "lavis_test_bot"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn repair_migrates_a_missing_id_only_after_matching_token_validation() {
        let (mut runtime, directory) = runtime_with_alias().await;
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let state_path = directory.join("setup.json");
        let token_path = directory.join("token");
        let mut state = PersistedSetupState::default();
        state.identities.bot_username = Some("lavis_test_bot".to_owned());
        let mut store = SetupStore::new(state_path.clone(), token_path.clone());
        store.save_state(&state).unwrap();
        store
            .save_token(&CompanionToken::new("123456:abcdefghijklmnopqrstUVWX".to_owned()).unwrap())
            .unwrap();
        runtime.configure_setup(
            state_path.clone(),
            token_path.clone(),
            PeerId::user(1).unwrap(),
        );
        let setup = runtime.setup.as_ref().unwrap();

        let wrong = RepairBotApi {
            identity: Ok(BotIdentity {
                id: 7,
                username: "other_bot".to_owned(),
            }),
        };
        assert!(
            setup
                .repair_preflight(&wrong, Locale::Russian)
                .await
                .is_err()
        );
        assert_eq!(
            SetupStore::new(state_path.clone(), token_path.clone())
                .load_state()
                .unwrap()
                .identities
                .bot_user_id,
            None
        );

        let matching = RepairBotApi {
            identity: Ok(BotIdentity {
                id: 7,
                username: "LAVIS_TEST_BOT".to_owned(),
            }),
        };
        assert!(
            setup
                .repair_preflight(&matching, Locale::Russian)
                .await
                .is_ok()
        );
        assert_eq!(
            SetupStore::new(state_path, token_path)
                .load_state()
                .unwrap()
                .identities
                .bot_user_id,
            Some(7)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn recorded_identity_blocks_new_botfather_flow_when_token_is_missing() {
        let (mut runtime, directory) = runtime_with_alias().await;
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let state_path = directory.join("setup.json");
        let token_path = directory.join("token");
        let mut state = PersistedSetupState::default();
        state.identities.bot_username = Some("lavis_test_bot".to_owned());
        state.identities.bot_user_id = Some(7);
        state.stages.bot_identity_recorded = true;
        SetupStore::new(state_path.clone(), token_path.clone())
            .save_state(&state)
            .unwrap();
        runtime.configure_setup(state_path, token_path, PeerId::user(1).unwrap());

        assert!(runtime.setup.as_ref().unwrap().has_created_bot().await);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn shows_existing_alias_with_utf16_safe_collapsed_body() {
        let (mut runtime, directory) = runtime_with_alias().await;
        runtime
            .settings
            .set_locale(Some(crate::i18n::Locale::English))
            .await
            .unwrap();
        let response = runtime
            .execute_alias(
                &AliasRequest::Show {
                    name: "MINI".to_owned(),
                },
                "🦀",
            )
            .await;
        let units = response.text.encode_utf16().collect::<Vec<_>>();
        let grammers_client::tl::enums::MessageEntity::Blockquote(entity) = &response.entities[0]
        else {
            panic!("expected a blockquote entity");
        };

        assert_eq!(
            response.text,
            "🔗 🦀mini\n\nAlias for:\n🦀fastfetch --separator ' → '"
        );
        assert_eq!(response.entities.len(), 1);
        assert!(entity.collapsed);
        let offset = usize::try_from(entity.offset).unwrap();
        let length = usize::try_from(entity.length).unwrap();
        assert_eq!(
            String::from_utf16(&units[..offset]).unwrap(),
            "🔗 🦀mini\n\n"
        );
        assert_eq!(
            String::from_utf16(&units[offset..offset + length]).unwrap(),
            "Alias for:\n🦀fastfetch --separator ' → '"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn reports_missing_alias_and_invalid_show_usage() {
        let (mut runtime, directory) = runtime_with_alias().await;
        runtime
            .settings
            .set_locale(Some(crate::i18n::Locale::English))
            .await
            .unwrap();

        assert_eq!(
            runtime
                .execute_alias(
                    &AliasRequest::Show {
                        name: "MISSING".to_owned()
                    },
                    "!"
                )
                .await,
            Response::plain("⚠️ Alias does not exist: !missing")
        );
        assert_eq!(
            runtime.execute_alias(&AliasRequest::Invalid, "!").await,
            Response::plain(
                "⚠️ Usage: !alias [list|add <name> <command> [arguments...]|show <name>|del <name>]"
            )
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn modules_overview_uses_current_prefix_and_invalid_usage() {
        let (mut runtime, directory) = runtime_with_alias().await;
        runtime
            .execute_prefix(&crate::commands::PrefixRequest::Set("🦀".to_owned()))
            .await;
        let overview =
            runtime.execute_modules(&crate::commands::ModulesRequest::Overview, runtime.prefix());
        let heading = format!("🧩 Модули Lavis: {}\n\n", crate::modules::modules().len());
        assert!(overview.text.starts_with(&heading));
        assert!(overview.text.contains("🦀fastfetch"));
        assert!(
            overview
                .text
                .contains(&format!("Команды ({})", crate::commands::commands().len()))
        );
        assert_eq!(overview.entities.len(), 2);
        assert_eq!(
            runtime.execute_modules(&crate::commands::ModulesRequest::Invalid, runtime.prefix(),),
            Response::plain("⚠️ Использование: 🦀modules")
        );
        let grammers_client::tl::enums::MessageEntity::Blockquote(entity) = &overview.entities[0]
        else {
            panic!("expected blockquote")
        };
        let units = overview.text.encode_utf16().collect::<Vec<_>>();
        let offset = usize::try_from(entity.offset).unwrap();
        let length = usize::try_from(entity.length).unwrap();
        assert_eq!(String::from_utf16(&units[..offset]).unwrap(), heading);
        let body = String::from_utf16(&units[offset..offset + length]).unwrap();
        assert!(body.contains(&format!("Команды ({})", crate::commands::commands().len())));
        let grammers_client::tl::enums::MessageEntity::Blockquote(provenance) =
            &overview.entities[1]
        else {
            panic!("expected provenance blockquote")
        };
        assert!(entity.collapsed);
        assert!(!provenance.collapsed);
        let provenance_offset = usize::try_from(provenance.offset).unwrap();
        let provenance_length = usize::try_from(provenance.length).unwrap();
        assert_eq!(
            String::from_utf16(&units[provenance_offset..provenance_offset + provenance_length])
                .unwrap(),
            "Это встроенный модуль Lavis. Его нельзя выгрузить или заменить."
        );

        runtime
            .settings
            .set_locale(Some(Locale::English))
            .await
            .unwrap();
        let english =
            runtime.execute_modules(&crate::commands::ModulesRequest::Overview, runtime.prefix());
        let english_heading = format!("🧩 Lavis modules: {}\n\n", crate::modules::modules().len());
        assert!(english.text.starts_with(&english_heading));
        assert!(
            english
                .text
                .contains(&format!("Commands ({})", crate::commands::commands().len()))
        );
        assert!(!english.text.contains("Модули"));
        assert_eq!(
            runtime.execute_modules(&crate::commands::ModulesRequest::Invalid, runtime.prefix(),),
            Response::plain("⚠️ Usage: 🦀modules")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn bounds_rejects_saturation_and_cleans_up_expected_self_edits() {
        let (mut runtime, directory) = runtime_with_alias().await;
        let peer = grammers_session::types::PeerId::user(1).unwrap();
        for message_id in 0..super::MAX_EXPECTED_SELF_EDITS as i32 {
            runtime
                .register_expected_self_edit(peer, message_id, format!("response {message_id}"))
                .unwrap();
        }
        assert!(
            runtime
                .register_expected_self_edit(peer, 128, "response 128".to_owned())
                .is_err()
        );
        assert!(runtime.consume_expected_self_edit(peer, 0, "response 0"));
        assert!(runtime.consume_expected_self_edit(peer, 1, "response 1"));

        runtime
            .register_expected_self_edit(peer, 42, "old response".to_owned())
            .unwrap();
        runtime
            .register_expected_self_edit(peer, 42, "new response".to_owned())
            .unwrap();
        assert!(runtime.consume_expected_self_edit(peer, 42, "old response"));
        assert!(runtime.consume_expected_self_edit(peer, 42, "new response"));

        runtime
            .register_expected_self_edit(peer, 43, "failed response".to_owned())
            .unwrap();
        runtime.remove_expected_self_edit(peer, 43, "failed response");
        assert!(!runtime.consume_expected_self_edit(peer, 43, "failed response"));

        runtime
            .register_expected_self_edit(peer, 44, "duplicate response".to_owned())
            .unwrap();
        runtime
            .register_expected_self_edit(peer, 44, "duplicate response".to_owned())
            .unwrap();
        assert!(runtime.consume_expected_self_edit(peer, 44, "duplicate response"));
        assert!(runtime.consume_expected_self_edit(peer, 44, "duplicate response"));
        assert!(!runtime.consume_expected_self_edit(peer, 44, "duplicate response"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn setup_edit_fallback_is_deduplicated_per_source() {
        let (mut runtime, directory) = runtime_with_alias().await;
        let first_peer = PeerId::user(1).unwrap();
        let second_peer = PeerId::user(2).unwrap();

        assert!(runtime.claim_setup_edit_fallback(first_peer, 7));
        assert!(!runtime.claim_setup_edit_fallback(first_peer, 7));
        assert!(runtime.claim_setup_edit_fallback(first_peer, 8));
        assert!(runtime.claim_setup_edit_fallback(second_peer, 7));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn prefix_changes_persist_reset_and_fail_without_changing_runtime_state() {
        let (mut runtime, directory) = runtime_with_alias().await;
        assert_eq!(runtime.prefix(), ",");
        assert_eq!(
            runtime
                .execute_prefix(&crate::commands::PrefixRequest::Set(".".to_owned()))
                .await
                .text,
            "⚙️ Префикс команд изменён: ."
        );
        assert_eq!(runtime.prefix(), ".");
        assert_eq!(
            crate::settings::SettingsStore::load(directory.join("settings.json"))
                .await
                .unwrap()
                .prefix(),
            "."
        );
        assert_eq!(
            runtime
                .execute_prefix(&crate::commands::PrefixRequest::Reset)
                .await
                .text,
            "⚙️ Префикс сброшен: ,"
        );
        assert_eq!(runtime.prefix(), ",");
        assert!(
            runtime
                .execute_prefix(&crate::commands::PrefixRequest::Set("bad".to_owned()))
                .await
                .text
                .contains("Не удалось изменить")
        );
        assert_eq!(runtime.prefix(), ",");
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn prefix_and_alias_responses_follow_the_locale_without_internal_errors() {
        let (mut runtime, directory) = runtime_with_alias().await;
        for (locale, current, usage, added, listed, shown, deleted, missing, invalid_alias) in [
            (
                Locale::English,
                "⚙️ Active prefix: ,",
                "⚠️ Usage: ,prefix [new-prefix|reset]",
                "🔗 Added alias: ,new",
                "🔗 Aliases\n\n,mini → ,fastfetch --separator ' → '\n,new → ,ping",
                "🔗 ,new\n\nAlias for:\n,ping",
                "🔗 Deleted alias: ,new",
                "❓ Alias not found: missing",
                "⚠️ Usage: ,alias [list|add <name> <command> [arguments...]|show <name>|del <name>]",
            ),
            (
                Locale::Russian,
                "⚙️ Текущий префикс: ,",
                "⚠️ Использование: ,prefix [new-prefix|reset]",
                "🔗 Добавлен псевдоним: ,new",
                "🔗 Псевдонимы\n\n,mini → ,fastfetch --separator ' → '\n,new → ,ping",
                "🔗 ,new\n\nПсевдоним для:\n,ping",
                "🔗 Псевдоним удалён: ,new",
                "❓ Псевдоним не найден: missing",
                "⚠️ Использование: ,alias [list|add <name> <command> [arguments...]|show <name>|del <name>]",
            ),
        ] {
            runtime.settings.set_locale(Some(locale)).await.unwrap();
            assert_eq!(
                runtime
                    .execute_prefix(&crate::commands::PrefixRequest::Show)
                    .await
                    .text,
                current
            );
            assert_eq!(
                runtime
                    .execute_prefix(&crate::commands::PrefixRequest::Invalid)
                    .await
                    .text,
                usage
            );
            assert_eq!(
                runtime
                    .execute_alias(
                        &AliasRequest::Add {
                            name: "new".to_owned(),
                            target: "ping".to_owned(),
                            args: vec![],
                        },
                        ",",
                    )
                    .await
                    .text,
                added
            );
            assert_eq!(
                runtime.execute_alias(&AliasRequest::List, ",").await.text,
                listed
            );
            assert_eq!(
                runtime
                    .execute_alias(
                        &AliasRequest::Show {
                            name: "new".to_owned()
                        },
                        ","
                    )
                    .await
                    .text,
                shown
            );
            assert_eq!(
                runtime
                    .execute_alias(
                        &AliasRequest::Delete {
                            name: "new".to_owned()
                        },
                        ","
                    )
                    .await
                    .text,
                deleted
            );
            assert_eq!(
                runtime
                    .execute_alias(
                        &AliasRequest::Delete {
                            name: "missing".to_owned()
                        },
                        ","
                    )
                    .await
                    .text,
                missing
            );
            assert_eq!(
                runtime
                    .execute_alias(&AliasRequest::Invalid, ",")
                    .await
                    .text,
                invalid_alias
            );
        }
        for (locale, prefix_error, alias_error) in [
            (
                Locale::English,
                "⚠️ Could not change prefix.",
                "⚠️ Could not add alias.",
            ),
            (
                Locale::Russian,
                "⚠️ Не удалось изменить префикс.",
                "⚠️ Не удалось добавить псевдоним.",
            ),
        ] {
            runtime.settings.set_locale(Some(locale)).await.unwrap();
            assert_eq!(
                runtime
                    .execute_prefix(&crate::commands::PrefixRequest::Set("bad".to_owned()))
                    .await
                    .text,
                prefix_error
            );
            assert_eq!(
                runtime
                    .execute_alias(
                        &AliasRequest::Add {
                            name: "ping".to_owned(),
                            target: "stats".to_owned(),
                            args: vec![],
                        },
                        ",",
                    )
                    .await
                    .text,
                alias_error
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn fastfetch_validation_and_failure_responses_follow_the_locale() {
        for (locale, invalid, failure) in [
            (
                Locale::English,
                "⚠️ Fastfetch input error: invalid --logo value. See !help fastfetch",
                "⚠️ Fastfetch timed out. See !help fastfetch",
            ),
            (
                Locale::Russian,
                "⚠️ Ошибка ввода Fastfetch: неверное значение --logo. См. !help fastfetch",
                "⚠️ Fastfetch превысил время ожидания. См. !help fastfetch",
            ),
        ] {
            assert_eq!(
                fastfetch_response(
                    FastfetchResult::InvalidArguments(FastfetchInputError::InvalidLogo),
                    locale,
                    "!",
                    std::path::Path::new("/tmp/fastfetch.json"),
                ),
                Response::plain(invalid)
            );
            assert_eq!(
                fastfetch_response(
                    FastfetchResult::TimedOut,
                    locale,
                    "!",
                    std::path::Path::new("/tmp/fastfetch.json"),
                ),
                Response::plain(failure)
            );
        }
    }

    #[test]
    fn reboot_and_sensitive_denial_responses_follow_the_locale() {
        for (locale, rebooting, reboot_denial, module_denial) in [
            (
                Locale::English,
                "♻️ Lavis is restarting…",
                "⚠️ Restart is available only from a new self-authored message.",
                "⚠️ This module operation is available only from a new self-authored message in Saved Messages.",
            ),
            (
                Locale::Russian,
                "♻️ Lavis перезапускается…",
                "⚠️ Перезапуск доступен только из нового собственного сообщения.",
                "⚠️ Эта операция с модулями доступна только из нового собственного сообщения в Saved Messages.",
            ),
        ] {
            assert_eq!(runtime_text(locale, RuntimeText::Rebooting), rebooting);
            for denial in [
                SensitiveCommandDenial::Edited,
                SensitiveCommandDenial::NotSelfAuthored,
                SensitiveCommandDenial::InvalidMessageId,
            ] {
                assert_eq!(
                    denial.response(locale, SensitiveCommandPolicy::Reboot),
                    reboot_denial
                );
            }
            assert_eq!(
                SensitiveCommandDenial::NotSavedMessages
                    .response(locale, SensitiveCommandPolicy::ModuleMutation),
                module_denial
            );
        }
    }

    #[test]
    fn formats_durations_compactly() {
        assert_eq!(format_duration(Duration::ZERO), "0s");
        assert_eq!(format_duration(Duration::from_millis(999)), "0s");
        assert_eq!(format_duration(Duration::from_secs(61)), "1m 01s");
        assert_eq!(format_duration(Duration::from_secs(3_661)), "1h 01m 01s");
        assert_eq!(
            format_duration(Duration::from_secs(183_845)),
            "2d 03h 04m 05s"
        );
    }

    #[test]
    fn formats_latency_in_milliseconds() {
        assert_eq!(format_latency(Duration::ZERO), "<1 ms");
        assert_eq!(format_latency(Duration::from_micros(999)), "<1 ms");
        assert_eq!(format_latency(Duration::from_millis(12)), "12 ms");
    }

    #[test]
    fn ping_success_and_failure_follow_the_locale() {
        assert_eq!(
            ping_text(Locale::English, PingText::Success, "12 ms"),
            "🏓 Pong: 12 ms"
        );
        assert_eq!(
            runtime_text(Locale::English, RuntimeText::PingFailed),
            "⚠️ Telegram ping failed"
        );
        assert_eq!(
            ping_text(Locale::Russian, PingText::Success, "12 ms"),
            "🏓 Понг: 12 ms"
        );
        assert_eq!(
            runtime_text(Locale::Russian, RuntimeText::PingFailed),
            "⚠️ Не удалось выполнить Telegram ping"
        );
    }

    #[test]
    fn categorizes_external_event_failures_without_exposing_error_details() {
        assert_eq!(
            external_event_error_category(&crate::error::ExternalError::Unavailable),
            "unavailable"
        );
        assert_eq!(
            external_event_error_category(&crate::error::ExternalError::ExecutionTimeout),
            "timeout"
        );
        assert_eq!(
            external_event_error_category(&crate::error::ExternalError::ProtocolDecode),
            "protocol"
        );
        assert_eq!(
            external_event_error_category(&crate::error::ExternalError::ModuleError),
            "module_error"
        );
        assert_eq!(
            external_event_error_category(&crate::error::ExternalError::NotReadable),
            "other"
        );
    }

    #[cfg(all(feature = "fixture-tests", unix))]
    #[tokio::test]
    async fn dispatches_independent_created_events_concurrently() {
        const EVENT_MODULE: &str = r#"#!/usr/bin/env python3
import json, os, sys, time
ready_dir = os.environ["READY_DIR"]
module_id = None
for line in sys.stdin:
    message = json.loads(line)
    request_id = message["request_id"]
    if message["type"] == "initialize":
        module_id = message["module_id"]
        response = {"protocol_version": message["protocol_version"], "type": "initialized", "request_id": request_id, "module_id": message["module_id"]}
    elif message["type"] == "event":
        open(os.path.join(ready_dir, "started-" + module_id), "w").close()
        while not os.path.exists(os.path.join(ready_dir, "release")):
            time.sleep(0.01)
        response = {"protocol_version": 3, "type": "event_result", "request_id": request_id, "actions": []}
    elif message["type"] == "shutdown":
        response = {"protocol_version": 3, "type": "health", "request_id": request_id}
    else:
        continue
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#;

        fn descriptor(
            id: &str,
            entrypoint: PathBuf,
            module_dir: PathBuf,
            protocol_version: u32,
            subscribed: bool,
        ) -> crate::external_modules::manifest::ExternalModuleDescriptor {
            crate::external_modules::manifest::ExternalModuleDescriptor {
                protocol_version,
                contract_revision: (protocol_version == 6).then_some(2),
                id: id.to_owned(),
                display_name: id.to_owned(),
                version: "test".to_owned(),
                author: "test".to_owned(),
                entrypoint,
                module_dir,
                capabilities: vec![
                    crate::external_modules::manifest::ExternalCapability::MessageRead,
                ],
                default_command: None,
                subscriptions: subscribed
                    .then_some(
                        crate::external_modules::manifest::ExternalSubscription::MessageCreated,
                    )
                    .into_iter()
                    .collect(),
                telegram_methods: vec![],
                actions: vec![],
                commands: vec![],
            }
        }

        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!("lavis-runtime-events-{nonce}-{seq}"));
        fs::create_dir_all(&directory).unwrap();
        let python = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|path| path.join("python3"))
            .find(|path| path.is_file())
            .expect("fixture tests require python3 in PATH");
        let mut descriptors = Vec::new();
        for (id, protocol_version, subscribed) in [
            ("first", 3, true),
            ("second", 3, true),
            ("legacy", 2, true),
            ("unsubscribed", 3, false),
        ] {
            let module_dir = directory.join(id);
            fs::create_dir_all(&module_dir).unwrap();
            let entrypoint = module_dir.join("module.py");
            fs::write(
                &entrypoint,
                EVENT_MODULE
                    .replacen(
                        "#!/usr/bin/env python3",
                        &format!("#!{}", python.display()),
                        1,
                    )
                    .replace(
                        "ready_dir = os.environ[\"READY_DIR\"]",
                        &format!("ready_dir = {:?}", directory),
                    ),
            )
            .unwrap();
            fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o700)).unwrap();
            descriptors.push(descriptor(
                id,
                entrypoint,
                module_dir,
                protocol_version,
                subscribed,
            ));
        }

        let mut manager = crate::external_modules::manager::ExternalManager::new();
        manager.set_descriptors(descriptors.clone());
        let handle = crate::external_modules::manager::ExternalManagerHandle::new(manager);
        handle
            .startup_enabled(
                &descriptors
                    .iter()
                    .map(|descriptor| descriptor.id.clone())
                    .collect(),
            )
            .await;
        let (mut runtime, state_directory) = runtime_with_alias().await;
        runtime.set_external_manager(handle.clone()).await;
        let dispatch = runtime
            .prepare_message_event_dispatch(
                PeerId::user(7).expect("valid test peer"),
                42,
                crate::external_modules::protocol::MessageEventKind::Created,
                "event",
                true,
                vec![],
            )
            .expect("only subscribed v3 modules should receive events");

        let execute = dispatch.execute();
        tokio::pin!(execute);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !directory.join("started-first").exists() || !directory.join("started-second").exists() {
                tokio::select! {
                    _ = &mut execute => panic!("event dispatch completed before both modules began"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
        })
        .await
        .expect("independent module requests did not begin together");
        assert!(!directory.join("started-legacy").exists());
        assert!(!directory.join("started-unsubscribed").exists());
        fs::write(directory.join("release"), "").unwrap();
        assert!(execute.await.actions.is_empty());
        handle.shutdown_all().await;
        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(state_directory).unwrap();
    }

    #[test]
    fn reports_fastfetch_exit_codes_without_stderr() {
        assert_eq!(
            fastfetch_response(
                FastfetchResult::NonZero {
                    code: 1,
                    stderr: "sensitive diagnostic".to_owned(),
                },
                Locale::English,
                "!",
                std::path::Path::new("/tmp/fastfetch.json"),
            )
            .text,
            "⚠️ Fastfetch failed (exit code 1). See !help fastfetch"
        );
    }

    #[tokio::test]
    async fn fastfetch_errors_use_prefix_and_malformed_aliases_are_visible() {
        let (runtime, directory) = runtime_with_alias().await;
        assert_eq!(
            fastfetch_response(
                FastfetchResult::InvalidArguments(FastfetchInputError::InvalidLogo),
                Locale::English,
                "🦀",
                std::path::Path::new("/tmp/fastfetch.json"),
            ),
            Response::plain("⚠️ Fastfetch input error: invalid --logo value. See 🦀help fastfetch")
        );
        let profile_path = PathBuf::from("/tmp/profile\nfastfetch.json");
        let response = fastfetch_response(
            FastfetchResult::ProfileError(FastfetchProfileError::Malformed),
            Locale::English,
            "🦀",
            &profile_path,
        );
        assert!(response.text.contains("profile is malformed"));
        assert!(response.text.contains(&format!("{profile_path:?}")));
        assert!(!response.text.contains("/tmp/profile\nfastfetch.json"));
        assert!(response.text.contains("🦀help fastfetch"));
        assert_eq!(
            runtime.resolve_alias("mini", "'"),
            Some(Action::Fastfetch("'".to_owned()))
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parses_valid_system_uptime() {
        assert_eq!(parse_system_uptime("0.00 0.00"), Some(Duration::ZERO));
        assert_eq!(
            parse_system_uptime("61.42 120.00"),
            Some(Duration::from_secs_f64(61.42))
        );
        assert_eq!(
            parse_system_uptime("183845.75 999999.99"),
            Some(Duration::from_secs_f64(183845.75))
        );
    }

    #[test]
    fn rejects_malformed_system_uptime() {
        assert_eq!(parse_system_uptime(""), None);
        assert_eq!(parse_system_uptime("NaN 1.0"), None);
        assert_eq!(parse_system_uptime("-1 1.0"), None);
        assert_eq!(parse_system_uptime("invalid"), None);
    }

    #[test]
    fn parses_memory_kib_with_extra_whitespace() {
        assert_eq!(
            parse_memory_kib("Name:\tlavis\nVmRSS:\t  1234 kB\n"),
            Some(1234)
        );
    }

    #[test]
    fn parses_rss_from_a_status_fixture_with_unrelated_fields() {
        let status = "Name:\tlavis\nVmSize:\t 20480 kB\nVmRSS: 10624 kB\nThreads:\t2\n";

        assert_eq!(parse_memory_kib(status), Some(10624));
    }

    #[test]
    fn rejects_missing_or_malformed_memory_kib() {
        assert_eq!(parse_memory_kib("Name:\tlavis\n"), None);
        assert_eq!(parse_memory_kib("VmRSS: bad kB\n"), None);
        assert_eq!(parse_memory_kib("VmRSS: 1234 bytes\n"), None);
    }

    #[tokio::test]
    async fn info_caption_computes_module_counts_from_runtime_state() {
        use crate::external_modules::manifest::{ExternalCapability, ExternalModuleDescriptor};

        fn descriptor(id: &str) -> ExternalModuleDescriptor {
            ExternalModuleDescriptor {
                protocol_version: 6,
                contract_revision: Some(2),
                id: id.to_owned(),
                display_name: id.to_owned(),
                version: "test".to_owned(),
                author: "test".to_owned(),
                entrypoint: PathBuf::from("/unused"),
                module_dir: PathBuf::from("/unused"),
                capabilities: vec![ExternalCapability::MessageRead],
                default_command: None,
                subscriptions: Vec::new(),
                telegram_methods: Vec::new(),
                actions: Vec::new(),
                commands: Vec::new(),
            }
        }

        let mut snapshot = ExternalRuntimeSnapshot::new();
        let built_in_modules = crate::modules::modules().len();
        snapshot.descriptors.push(descriptor("alpha"));
        snapshot.descriptors.push(descriptor("beta"));
        snapshot.module_statuses.push(ExternalModuleStatus {
            id: "alpha".to_owned(),
            display_name: "alpha".to_owned(),
            version: "test".to_owned(),
            author: "test".to_owned(),
            capabilities: Vec::new(),
            command_count: 1,
            status: ExternalModuleRuntimeStatus::Running,
        });
        snapshot.module_statuses.push(ExternalModuleStatus {
            id: "beta".to_owned(),
            display_name: "beta".to_owned(),
            version: "test".to_owned(),
            author: "test".to_owned(),
            capabilities: Vec::new(),
            command_count: 1,
            status: ExternalModuleRuntimeStatus::InstalledDisabled,
        });

        let (mut runtime, directory) = runtime_with_alias().await;
        runtime.set_external_snapshot_for_tests(snapshot);
        runtime
            .settings
            .set_locale(Some(crate::i18n::Locale::English))
            .await
            .unwrap();
        let execution = runtime.execute_info();

        assert!(execution.response.text.contains(&format!(
            "Modules: {} ({} active)",
            built_in_modules + 2,
            built_in_modules + 1
        )));
        assert!(execution.response.text.contains("Host: "));
        assert!(execution.response.text.contains("OS: "));
        fs::remove_dir_all(directory).ok();
    }

    #[tokio::test]
    async fn info_action_dispatches_through_execute() {
        use grammers_session::types::PeerId;
        let (mut runtime, directory) = runtime_with_alias().await;
        runtime
            .settings
            .set_locale(Some(crate::i18n::Locale::English))
            .await
            .unwrap();
        runtime.set_self_identity(crate::auth::SelfIdentity {
            username: Some("owner".to_owned()),
            display_name: None,
            id: PeerId::self_user(),
        });
        runtime.publish_upstream_revision(Some(crate::upstream::UpstreamRevision {
            revision: "b1d18f8ef407d043506c983b0d68e96c282eb1c9".to_owned(),
            relation: crate::upstream::RevisionRelation::Current,
            version: Some("1.0.0".to_owned()),
        }));

        let execution = runtime.execute_info();

        assert!(execution.response.text.contains("Owner: @owner"));
        assert!(execution.response.text.contains(&format!(
            "Version: {} (current ✅)",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(execution.response.text.contains("Current commit: "));
        assert!(execution.response.text.contains("Upstream main: b1d18f8"));
        assert!(execution.response.text.contains("Prefix: "));
        assert!(execution.response.text.contains("Modules: "));
        assert_eq!(execution.response.entities.len(), 4);
        assert_eq!(
            execution.media.as_deref(),
            Some(crate::info::INFO_MEDIA_URL)
        );
        fs::remove_dir_all(directory).ok();
    }

    #[tokio::test]
    async fn info_reports_unavailable_upstream_when_resolver_fails() {
        let (mut runtime, directory) = runtime_with_alias().await;
        runtime
            .settings
            .set_locale(Some(crate::i18n::Locale::English))
            .await
            .unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        runtime.set_upstream(Box::new(PanickingUpstream {
            calls: calls.clone(),
        }));

        let execution = runtime.execute_info();

        assert!(
            execution
                .response
                .text
                .contains("Upstream main: unavailable")
        );
        assert!(
            execution
                .response
                .text
                .contains(&format!("Version: {}", env!("CARGO_PKG_VERSION")))
        );
        assert!(
            !execution
                .response
                .text
                .contains(&format!("Version: {} (", env!("CARGO_PKG_VERSION")))
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(execution.media.is_some());
        fs::remove_dir_all(directory).ok();
    }

    #[tokio::test]
    async fn upstream_failure_retains_the_last_successful_info_value() {
        let (mut runtime, directory) = runtime_with_alias().await;
        runtime
            .settings
            .set_locale(Some(crate::i18n::Locale::English))
            .await
            .unwrap();
        runtime.publish_upstream_revision(Some(crate::upstream::UpstreamRevision {
            revision: "b1d18f8ef407d043506c983b0d68e96c282eb1c9".to_owned(),
            relation: crate::upstream::RevisionRelation::Current,
            version: Some("1.0.0".to_owned()),
        }));
        runtime.publish_upstream_failure("timeout");

        let execution = runtime.execute_info();

        assert!(execution.response.text.contains("b1d18f8"));
        assert!(matches!(
            &runtime.upstream_revision_cache,
            UpstreamSnapshot::LastFailure {
                stale: Some(_),
                category
            } if category == "timeout"
        ));
        fs::remove_dir_all(directory).ok();
    }

    #[tokio::test]
    async fn upstream_snapshot_failure_and_recovery_transitions_are_explicit() {
        let (mut runtime, directory) = runtime_with_alias().await;
        assert!(matches!(
            &runtime.upstream_revision_cache,
            UpstreamSnapshot::Never
        ));

        runtime.publish_upstream_failure("transport");
        assert!(matches!(
            &runtime.upstream_revision_cache,
            UpstreamSnapshot::LastFailure { stale: None, .. }
        ));

        let revision = crate::upstream::UpstreamRevision {
            revision: "b1d18f8ef407d043506c983b0d68e96c282eb1c9".to_owned(),
            relation: crate::upstream::RevisionRelation::Current,
            version: Some("1.0.0".to_owned()),
        };
        runtime.publish_upstream_revision(Some(revision.clone()));
        runtime.publish_upstream_failure("timeout");
        assert!(matches!(
            &runtime.upstream_revision_cache,
            UpstreamSnapshot::LastFailure { stale: Some(_), .. }
        ));
        runtime.publish_upstream_revision(Some(revision));
        assert!(matches!(
            &runtime.upstream_revision_cache,
            UpstreamSnapshot::Success(_)
        ));
        fs::remove_dir_all(directory).ok();
    }

    #[tokio::test]
    async fn version_failure_retains_revision_but_successful_none_clears_version() {
        let (mut runtime, directory) = runtime_with_alias().await;
        runtime
            .settings
            .set_locale(Some(crate::i18n::Locale::English))
            .await
            .unwrap();
        runtime.publish_upstream_revision(Some(crate::upstream::UpstreamRevision {
            revision: "b1d18f8ef407d043506c983b0d68e96c282eb1c9".to_owned(),
            relation: crate::upstream::RevisionRelation::Ahead { commits: 2 },
            version: Some("99.0.0".to_owned()),
        }));
        runtime.publish_upstream_failure("rate_limited");

        // A failed version lookup publishes nothing: the stale relation and
        // successful version remain available to the local info command.
        let retained = runtime.execute_info();
        assert!(retained.response.text.contains("Upstream main: b1d18f8"));
        assert!(
            retained
                .response
                .text
                .contains(&format!("Version: {}", env!("CARGO_PKG_VERSION")))
        );
        assert!(
            retained
                .response
                .text
                .contains("newer available: 99.0.0 ⬆️")
        );

        // An actual successful lookup with no package version is different from
        // a failure and clears the previously known version.
        runtime.publish_upstream_version(None);
        let cleared = runtime.execute_info();
        assert!(cleared.response.text.contains("Upstream main: b1d18f8"));
        assert!(
            cleared
                .response
                .text
                .contains(&format!("Version: {}", env!("CARGO_PKG_VERSION")))
        );
        assert!(
            !cleared
                .response
                .text
                .contains(&format!("Version: {} (", env!("CARGO_PKG_VERSION")))
        );
        fs::remove_dir_all(directory).ok();
    }

    #[tokio::test]
    async fn info_owner_falls_back_to_unknown_without_identity() {
        let (mut runtime, directory) = runtime_with_alias().await;
        runtime
            .settings
            .set_locale(Some(crate::i18n::Locale::English))
            .await
            .unwrap();

        let execution = runtime.execute_info();

        assert!(execution.response.text.contains("Owner: unknown"));
        fs::remove_dir_all(directory).ok();
    }

    struct PanickingUpstream {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl UpstreamRev for PanickingUpstream {
        fn main_rev<'a>(&'a self) -> UpstreamRevFuture<'a> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("execute_info must not resolve upstream revisions")
        }

        fn compare<'a>(&'a self, _base: &'a str, _head: &'a str) -> CompareFuture<'a> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("execute_info must not compare upstream revisions")
        }
    }

    /// Regression coverage for blocker 1 at the resolution level: scripts a
    /// per-direction compare answer and records every ordered `(rev1, rev2)`
    /// pair requested, so a swapped orientation fails the exact call-order
    /// assertions instead of silently producing inverted relations.
    struct ScriptedRelationUpstream {
        main_rev: Result<String, UpstreamError>,
        compares: HashMap<(String, String), Result<crate::upstream::CompareResult, UpstreamError>>,
        requested: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl ScriptedRelationUpstream {
        fn requested_pairs(&self) -> Vec<(String, String)> {
            self.requested.lock().unwrap().clone()
        }
    }

    impl UpstreamRev for ScriptedRelationUpstream {
        fn main_rev<'a>(&'a self) -> UpstreamRevFuture<'a> {
            Box::pin(async move { self.main_rev.clone() })
        }

        fn compare<'a>(&'a self, base: &'a str, head: &'a str) -> CompareFuture<'a> {
            self.requested
                .lock()
                .unwrap()
                .push((base.to_owned(), head.to_owned()));
            let outcome = self
                .compares
                .get(&(base.to_owned(), head.to_owned()))
                .cloned()
                .unwrap_or(Err(UpstreamError::InvalidResponse));
            Box::pin(async move { outcome })
        }
    }

    /// ```text
    /// A -- B -- C
    ///      main  current
    /// ```
    #[tokio::test]
    async fn resolves_ahead_from_a_single_ordered_compare() {
        let upstream = ScriptedRelationUpstream {
            main_rev: Ok("B".to_owned()),
            compares: HashMap::from([(
                ("B".to_owned(), "C".to_owned()),
                Ok(crate::upstream::CompareResult {
                    ahead_by: 1,
                    behind_by: 0,
                    merge_base: Some("B".to_owned()),
                }),
            )]),
            requested: std::sync::Mutex::new(Vec::new()),
        };

        let resolved = resolve_upstream_relation_for(&upstream, "C").await.unwrap();

        assert_eq!(resolved.revision, "B");
        assert_eq!(
            resolved.relation,
            crate::upstream::RevisionRelation::Ahead { commits: 1 }
        );
        // merge_base == main proves Ahead: the reverse question must not even
        // be asked, and certainly not with swapped arguments.
        assert_eq!(
            upstream.requested_pairs(),
            vec![("B".to_owned(), "C".to_owned())]
        );
    }

    /// ```text
    /// A -- B -- C
    ///   current  main
    /// ```
    #[tokio::test]
    async fn resolves_behind_via_the_reverse_ordered_compare() {
        let upstream = ScriptedRelationUpstream {
            main_rev: Ok("C".to_owned()),
            compares: HashMap::from([
                (
                    ("C".to_owned(), "B".to_owned()),
                    Ok(crate::upstream::CompareResult {
                        ahead_by: 0,
                        behind_by: 0,
                        merge_base: Some("B".to_owned()),
                    }),
                ),
                (
                    ("B".to_owned(), "C".to_owned()),
                    Ok(crate::upstream::CompareResult {
                        ahead_by: 1,
                        behind_by: 0,
                        merge_base: Some("B".to_owned()),
                    }),
                ),
            ]),
            requested: std::sync::Mutex::new(Vec::new()),
        };

        let resolved = resolve_upstream_relation_for(&upstream, "B").await.unwrap();

        assert_eq!(resolved.revision, "C");
        assert_eq!(
            resolved.relation,
            crate::upstream::RevisionRelation::Behind { commits: 1 }
        );
        assert_eq!(
            upstream.requested_pairs(),
            vec![
                ("C".to_owned(), "B".to_owned()),
                ("B".to_owned(), "C".to_owned()),
            ]
        );
    }

    /// ```text
    ///       C -- D   current
    ///      /
    /// A -- B
    ///      \
    ///       E -- F -- G   main
    /// ```
    #[tokio::test]
    async fn resolves_divergence_as_ahead_two_behind_three() {
        let upstream = ScriptedRelationUpstream {
            main_rev: Ok("G".to_owned()),
            compares: HashMap::from([
                (
                    ("G".to_owned(), "D".to_owned()),
                    Ok(crate::upstream::CompareResult {
                        ahead_by: 2,
                        behind_by: 0,
                        merge_base: Some("B".to_owned()),
                    }),
                ),
                (
                    ("D".to_owned(), "G".to_owned()),
                    Ok(crate::upstream::CompareResult {
                        ahead_by: 3,
                        behind_by: 0,
                        merge_base: Some("B".to_owned()),
                    }),
                ),
            ]),
            requested: std::sync::Mutex::new(Vec::new()),
        };

        let resolved = resolve_upstream_relation_for(&upstream, "D").await.unwrap();

        assert_eq!(
            resolved.relation,
            crate::upstream::RevisionRelation::Diverged {
                ahead: 2,
                behind: 3
            }
        );
        assert_eq!(
            upstream.requested_pairs(),
            vec![
                ("G".to_owned(), "D".to_owned()),
                ("D".to_owned(), "G".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn identical_and_unknown_revisions_skip_compare_entirely() {
        let mut upstream = ScriptedRelationUpstream {
            main_rev: Ok("M".to_owned()),
            compares: HashMap::new(),
            requested: std::sync::Mutex::new(Vec::new()),
        };

        let current = resolve_upstream_relation_for(&upstream, "M").await.unwrap();
        assert_eq!(current.relation, crate::upstream::RevisionRelation::Current);

        upstream.main_rev = Ok("N".to_owned());
        let unknown = resolve_upstream_relation_for(&upstream, "unknown")
            .await
            .unwrap();
        assert_eq!(
            unknown.relation,
            crate::upstream::RevisionRelation::Unavailable
        );
        assert!(upstream.requested_pairs().is_empty());
    }

    #[tokio::test]
    async fn unpublished_current_revision_reports_unavailable() {
        let upstream = ScriptedRelationUpstream {
            main_rev: Ok("M".to_owned()),
            compares: HashMap::from([(
                ("M".to_owned(), "local".to_owned()),
                Err(UpstreamError::RevisionNotFound {
                    revision: "local".to_owned(),
                }),
            )]),
            requested: std::sync::Mutex::new(Vec::new()),
        };

        let resolved = resolve_upstream_relation_for(&upstream, "local")
            .await
            .unwrap();
        assert_eq!(
            resolved.relation,
            crate::upstream::RevisionRelation::Unavailable
        );
    }

    #[tokio::test]
    async fn rate_limited_compare_surfaces_rate_limit_failure() {
        let upstream = ScriptedRelationUpstream {
            main_rev: Ok("M".to_owned()),
            compares: HashMap::from([(
                ("M".to_owned(), "local".to_owned()),
                Err(UpstreamError::RateLimited { retry_after: None }),
            )]),
            requested: std::sync::Mutex::new(Vec::new()),
        };

        assert!(matches!(
            resolve_upstream_relation_for(&upstream, "local").await,
            Err(UpstreamResolveFailure::RateLimited { retry_after: None })
        ));
    }

    #[test]
    fn formats_stats_with_all_labels_and_values() {
        let output = format_stats(
            Locale::English,
            "12 ms",
            Duration::from_secs(61),
            &ProcStats {
                system_uptime: Some(Duration::from_secs(3_600)),
                memory_kib: Some(10_650),
            },
            2,
        );

        assert!(output.contains("📊 Lavis stats"));
        assert!(output.contains("Telegram: 12 ms"));
        assert!(output.contains("📊 Lavis stats\n\nTelegram"));
        assert!(output.contains("Lavis uptime: 1m 01s"));
        assert!(output.contains("System uptime: 1h 00m 00s"));
        assert!(output.contains("Memory: 10.4 MiB RSS"));
        assert!(output.contains("Commands: 2"));
        assert!(output.contains(&format!("Version: {}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn stats_unavailable_values_follow_the_locale() {
        let unavailable = ProcStats::default();
        let english = format_stats(
            Locale::English,
            "unavailable",
            Duration::from_secs(1),
            &unavailable,
            0,
        );
        assert!(english.contains("📊 Lavis stats"));
        assert!(english.contains("Telegram: unavailable"));
        assert!(english.contains("System uptime: unavailable"));
        assert!(english.contains("Memory: unavailable"));

        let russian = format_stats(
            Locale::Russian,
            "недоступно",
            Duration::from_secs(1),
            &unavailable,
            0,
        );
        assert!(russian.contains("📊 Статистика Lavis"));
        assert!(russian.contains("Telegram: недоступно"));
        assert!(russian.contains("Время работы системы: недоступно"));
        assert!(russian.contains("Память: недоступно"));
    }

    #[test]
    fn module_install_lists_are_bounded_and_mutation_denial_text_is_exact() {
        let values = (0..10)
            .map(|index| format!("value-{index}"))
            .collect::<Vec<_>>();
        assert_eq!(
            bounded_list(Locale::Russian, &values),
            "value-0, value-1, value-2, value-3, value-4, value-5, value-6, value-7, ещё 2"
        );
        assert_eq!(
            sensitive_text(Locale::Russian, SensitiveText::ModuleMutationDenied),
            "⚠️ Эта операция с модулями доступна только из нового собственного сообщения в Saved Messages."
        );
    }

    #[test]
    fn lm_install_gate_accepts_self_authored_saved_message_when_outgoing_is_false() {
        let self_user_id = PeerId::user(1).unwrap();
        let outgoing = false;

        assert!(!outgoing);
        assert_eq!(
            authorize_sensitive_message(
                SensitiveCommandPolicy::ModuleMutation,
                false,
                true,
                self_user_id,
                1,
                Some(self_user_id),
            ),
            Ok(())
        );
    }

    #[test]
    fn lm_install_gate_rejects_nonfresh_wrong_peer_and_nonself_messages() {
        let self_user_id = PeerId::user(1).unwrap();
        let other_user_id = PeerId::user(2).unwrap();

        assert_eq!(
            authorize_sensitive_message(
                SensitiveCommandPolicy::ModuleMutation,
                false,
                true,
                self_user_id,
                0,
                Some(self_user_id),
            ),
            Err(SensitiveCommandDenial::InvalidMessageId)
        );
        assert_eq!(
            authorize_sensitive_message(
                SensitiveCommandPolicy::ModuleMutation,
                false,
                true,
                other_user_id,
                1,
                Some(self_user_id),
            ),
            Err(SensitiveCommandDenial::NotSavedMessages)
        );
        assert_eq!(
            authorize_sensitive_message(
                SensitiveCommandPolicy::ModuleMutation,
                false,
                false,
                self_user_id,
                1,
                Some(self_user_id),
            ),
            Err(SensitiveCommandDenial::NotSelfAuthored)
        );
    }

    #[test]
    fn lm_usage_lists_each_supported_form() {
        assert_eq!(
            lm_usage(Locale::Russian, "."),
            "⚠️ Использование:\n.lm\n.lm list\n.lm info <id>\n.lm logs <id>\n.lm doctor [<id>]\n.lm install\n.lm confirm <ApprovalId>\n.lm cancel <ApprovalId>\n.lm enable <id>\n.lm disable <id>"
        );
    }

    #[tokio::test]
    async fn bare_lm_and_lm_list_empty_responses_follow_the_locale() {
        let (mut runtime, directory) = runtime_with_alias().await;
        runtime.configure_module_control(
            directory.join("modules"),
            directory.join("state.json"),
            directory.join("declarative.json"),
            PeerId::user(1).unwrap(),
        );

        let russian = Response::plain(
            "📦 Внешние модули не установлены.\n\nПрикрепите .lmod к сообщению, затем используйте:\n,lm install",
        );
        assert_eq!(runtime.render_lm_list().await, russian);
        assert_eq!(runtime.render_lm_list().await, russian);

        runtime
            .settings
            .set_locale(Some(Locale::English))
            .await
            .unwrap();
        let english = Response::plain(
            "📦 No external modules are installed.\n\nAttach a .lmod document to a message, then use:\n,lm install",
        );
        assert_eq!(runtime.render_lm_list().await, english);
        assert_eq!(runtime.render_lm_list().await, english);

        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn invalid_lm_usage_response_follows_the_locale() {
        let (mut runtime, directory) = runtime_with_alias().await;
        assert_eq!(
            runtime.lm_invalid_usage_response(),
            Response::plain(
                "⚠️ Использование:\n,lm\n,lm list\n,lm info <id>\n,lm logs <id>\n,lm doctor [<id>]\n,lm install\n,lm confirm <ApprovalId>\n,lm cancel <ApprovalId>\n,lm enable <id>\n,lm disable <id>"
            )
        );

        runtime
            .settings
            .set_locale(Some(Locale::English))
            .await
            .unwrap();
        assert_eq!(
            runtime.lm_invalid_usage_response(),
            Response::plain(
                "⚠️ Usage:\n,lm\n,lm list\n,lm info <id>\n,lm logs <id>\n,lm doctor [<id>]\n,lm install\n,lm confirm <ApprovalId>\n,lm cancel <ApprovalId>\n,lm enable <id>\n,lm disable <id>"
            )
        );

        fs::remove_dir_all(directory).unwrap();
    }
}
