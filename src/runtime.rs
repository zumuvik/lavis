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
        lm_state_text, lm_text, ping_text, prefix_text, render_lm_doctor_missing_catalog,
        render_lm_doctor_module, render_lm_doctor_report, render_lm_info, render_lm_install_plan,
        render_info_text, render_stats_text, runtime_text, sensitive_text, setup_text, stats_text,
        text,
    },
    info,
    onboarding::OnboardingProgress,
    response::{Response, sanitize_external_output},
    settings::{DEFAULT_PREFIX, SettingsStore},
    setup::{self, UsernameCandidate},
    setup_store::SetupStore,
    setup_telegram::{BotFatherProgress, CompanionSetup, GrammersTelegramSetup, ProvisionRequest},
    upstream::UpstreamRev,
};

pub struct RuntimeState {
    started_at: Instant,
    recognized_commands: u64,
    aliases: AliasStore,
    settings: SettingsStore,
    fastfetch_profile_path: PathBuf,
    self_identity: Option<SelfIdentity>,
    upstream: Option<Box<dyn UpstreamRev>>,
    upstream_main_rev_cache: Option<(Instant, String)>,
    external_manager: Option<ExternalManagerHandle>,
    external_snapshot: ExternalRuntimeSnapshot,
    expected_self_edits: VecDeque<ExpectedSelfEdit>,
    setup_notification_ids: VecDeque<(PeerId, i32)>,
    setup_edit_fallback_sources: VecDeque<(PeerId, i32)>,
    setup: Option<SetupCoordinator>,
    // Projection is held closed after setup is configured until BotFather's
    // authoritative peer identity has been resolved for this process.
    external_projection_permitted: bool,
    module_installation: Option<ModuleInstallation>,
    module_control: Option<ModuleControlConfig>,
    module_approvals: ApprovalStore<SystemClock, OsRandom>,
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

pub struct CreatedEventDispatch {
    handle: ExternalManagerHandle,
    requests: Vec<CreatedEventRequest>,
}

struct CreatedEventRequest {
    descriptor: crate::external_modules::manifest::ExternalModuleDescriptor,
    message_ref: String,
    event: MessageEventKind,
    payload: MessageEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedEventDispatchFailure {
    pub module_id: String,
    pub category: &'static str,
}

#[derive(Debug, Default)]
pub struct CreatedEventDispatchResult {
    pub actions: Vec<EventAction>,
    pub failures: Vec<CreatedEventDispatchFailure>,
}

impl CreatedEventDispatch {
    pub async fn execute(self) -> CreatedEventDispatchResult {
        let mut result = CreatedEventDispatchResult::default();
        let dispatches = self.requests.into_iter().map(|request| {
            let handle = self.handle.clone();
            async move {
                let CreatedEventRequest {
                    descriptor,
                    message_ref,
                    event,
                    payload,
                } = request;
                let module_id = descriptor.id.clone();
                let response = handle.dispatch_event(&module_id, event, payload).await;
                (descriptor, message_ref, response)
            }
        });

        for (descriptor, message_ref, response) in join_all(dispatches).await {
            let module_id = descriptor.id.clone();
            match response {
                Ok((request_id, actions)) => {
                    let scope = EventScope {
                        module_id: module_id.clone(),
                        request_id: request_id.clone(),
                        message_ref,
                    };
                    for action in actions {
                        if let Err(category) =
                            validate_reaction_action(&descriptor, &scope, &request_id, &action)
                        {
                            tracing::warn!(event = "external_reaction_rejected", ?category, module_id = %module_id, "External reaction action rejected");
                            continue;
                        }
                        result.actions.push(action);
                    }
                }
                Err(error) => {
                    result.failures.push(CreatedEventDispatchFailure {
                        module_id,
                        category: external_event_error_category(&error),
                    });
                }
            }
        }
        result
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExpectedSelfEdit {
    peer_id: PeerId,
    message_id: i32,
    text: String,
}

struct SetupCoordinator {
    state_path: PathBuf,
    token_path: PathBuf,
    saved_messages_peer: PeerId,
    botfather_peer: Option<PeerId>,
    phase: SetupPhase,
}

enum SetupPhase {
    Idle,
    AwaitingUsername {
        automatic: bool,
        deadline: Instant,
    },
    AwaitingConfirmation {
        username: UsernameCandidate,
        automatic: bool,
        attempts: u8,
        deadline: Instant,
    },
    Running {
        flow: CompanionSetup,
        transport: GrammersTelegramSetup,
        automatic: bool,
        attempts: u8,
        deadline: Instant,
    },
}

pub(crate) enum SetupInput {
    Ignored,
    Consumed {
        response: Option<Response>,
        provision: Option<ProvisionRequest>,
    },
}

struct BotFatherOutcome {
    response: Option<Response>,
    provision: Option<ProvisionRequest>,
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
    pub media: Option<PathBuf>,
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

#[derive(Clone, Copy)]
pub(crate) struct MessageExecutionContext<'a> {
    pub(crate) message: &'a Message,
    pub(crate) edited: bool,
    pub(crate) authored_by_self: bool,
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

fn stable_message_key(peer_id: PeerId, message_id: i32, module_id: &str) -> String {
    fn digest(domain: &str, peer_id: PeerId, message_id: i32, module_id: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        domain.hash(&mut hasher);
        peer_id.hash(&mut hasher);
        message_id.hash(&mut hasher);
        module_id.hash(&mut hasher);
        hasher.finish()
    }
    format!(
        "{:016x}{:016x}",
        digest("lavis-message-key-v1/a", peer_id, message_id, module_id),
        digest("lavis-message-key-v1/b", peer_id, message_id, module_id)
    )
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
            upstream_main_rev_cache: None,
            external_manager: None,
            external_snapshot: ExternalRuntimeSnapshot::new(),
            expected_self_edits: VecDeque::new(),
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

    /// Resolves the upstream `main` revision once per TTL. Failures are cached
    /// as an empty string so repeated `info` invocations do not hammer the
    /// endpoint; the caption renders the localized "unavailable" for it.
    async fn upstream_main_rev(&mut self) -> String {
        const UPSTREAM_MAIN_REV_TTL: Duration = Duration::from_secs(300);
        if let Some((resolved_at, revision)) = self.upstream_main_rev_cache.as_ref()
            && resolved_at.elapsed() < UPSTREAM_MAIN_REV_TTL
        {
            return revision.clone();
        }
        let resolved = match &self.upstream {
            Some(upstream) => match upstream.main_rev().await {
                Ok(revision) => revision,
                Err(error) => {
                    tracing::warn!(
                        event = "upstream_main_rev_unavailable",
                        ?error,
                        "Could not resolve the upstream main revision"
                    );
                    String::new()
                }
            },
            None => {
                tracing::warn!(
                    event = "upstream_resolver_unavailable",
                    "No upstream resolver is configured"
                );
                String::new()
            }
        };
        self.upstream_main_rev_cache = Some((Instant::now(), resolved.clone()));
        resolved
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

    pub fn prepare_message_event_dispatch(
        &self,
        peer_id: PeerId,
        message_id: i32,
        event: MessageEventKind,
        text: &str,
        outgoing: bool,
        entities: Vec<crate::external_modules::protocol::CustomEmojiEntity>,
    ) -> Option<CreatedEventDispatch> {
        if !self.external_projection_permitted {
            return None;
        }
        let handle = self.external_manager.clone()?;
        let mut requests = Vec::new();
        for descriptor in self
            .external_snapshot
            .descriptors
            .iter()
            .filter(|descriptor| module_can_receive_event(descriptor, event))
        {
            let event_id = crate::external_modules::protocol::request_id();
            let Ok(message_ref) = opaque_message_ref() else {
                tracing::warn!(event = "external_event_reference_failed", module_id = %descriptor.id, "Could not create an external event reference");
                continue;
            };
            let payload = MessageEvent {
                event_id,
                message_ref: message_ref.clone(),
                message_key: stable_message_key(peer_id, message_id, &descriptor.id),
                peer_id: descriptor
                    .capabilities
                    .contains(&ExternalCapability::MessagePeerId)
                    .then(|| peer_id.bot_api_dialog_id())
                    .flatten(),
                text: text.to_owned(),
                outgoing,
                entities: entities.clone(),
            };
            requests.push(CreatedEventRequest {
                descriptor: descriptor.clone(),
                message_ref,
                event,
                payload,
            });
        }
        (!requests.is_empty()).then_some(CreatedEventDispatch { handle, requests })
    }

    pub fn external_command_refs(&self) -> &[crate::external_modules::manager::ExternalCommandRef] {
        &self.external_snapshot.command_refs
    }

    pub fn has_active_external_command(&self, name: &str) -> bool {
        self.external_snapshot.active_commands.contains(name)
    }

    pub fn external_descriptors(
        &self,
    ) -> &[crate::external_modules::manifest::ExternalModuleDescriptor] {
        &self.external_snapshot.descriptors
    }

    pub fn prefix(&self) -> &str {
        self.settings.prefix()
    }

    pub(crate) fn locale(&self) -> Locale {
        self.settings.locale().unwrap_or(Locale::Russian)
    }

    pub fn register_expected_self_edit(&mut self, peer_id: PeerId, message_id: i32, text: String) {
        self.expected_self_edits.retain(|expected| {
            expected.peer_id != peer_id
                || expected.message_id != message_id
                || expected.text != text
        });
        if self.expected_self_edits.len() == MAX_EXPECTED_SELF_EDITS {
            self.expected_self_edits.pop_front();
        }
        self.expected_self_edits.push_back(ExpectedSelfEdit {
            peer_id,
            message_id,
            text,
        });
    }

    pub fn consume_expected_self_edit(
        &mut self,
        peer_id: PeerId,
        message_id: i32,
        text: &str,
    ) -> bool {
        let Some(index) = self.expected_self_edits.iter().position(|expected| {
            expected.peer_id == peer_id
                && expected.message_id == message_id
                && expected.text == text
        }) else {
            return false;
        };
        self.expected_self_edits.remove(index);
        true
    }

    pub fn remove_expected_self_edit(&mut self, peer_id: PeerId, message_id: i32, text: &str) {
        self.consume_expected_self_edit(peer_id, message_id, text);
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

    pub fn resolve_external(&self, name: &str, args: &str) -> Option<Action> {
        if !self.has_active_external_command(name) {
            return None;
        }
        let dot = name.find('.')?;
        let module_id = name[..dot].to_owned();
        let command_name = name[dot + 1..].to_owned();
        Some(Action::External(ExternalInvocation {
            module_id,
            command_name,
            arguments: if args.is_empty() {
                String::new()
            } else {
                args.to_owned()
            },
            argument_entities: Vec::new(),
        }))
    }

    pub fn resolve_external_default(&self, name: &str, args: &str) -> Option<Action> {
        let command_name = self.external_snapshot.active_defaults.get(name)?.clone();
        Some(Action::External(ExternalInvocation {
            module_id: name.to_owned(),
            command_name,
            arguments: args.to_owned(),
            argument_entities: Vec::new(),
        }))
    }

    pub fn has_external_module(&self, module_id: &str) -> bool {
        self.external_snapshot
            .descriptors
            .iter()
            .any(|descriptor| descriptor.id == module_id)
    }

    async fn execute_external(&mut self, invocation: &ExternalInvocation) -> Response {
        let locale = self.locale();
        let handle = match self.external_manager.clone() {
            Some(h) => h,
            None => {
                return Response::plain_with_locale(
                    self.locale(),
                    external_command_text(
                        locale,
                        ExternalCommandText::RuntimeUnavailable,
                        "",
                        None,
                    ),
                );
            }
        };
        let result = handle
            .execute(
                &invocation.module_id,
                &invocation.command_name,
                &invocation.arguments,
                &invocation.argument_entities,
            )
            .await;
        let response = match &result {
            Ok(text) => {
                let found = self
                    .external_snapshot
                    .descriptors
                    .iter()
                    .find(|d| d.id == invocation.module_id);
                match found {
                    Some(desc) => Response::external_result(
                        self.locale(),
                        text,
                        &desc.display_name,
                        &desc.id,
                        &desc.version,
                    ),
                    None => missing_descriptor_response(locale, text, &invocation.module_id),
                }
            }
            Err(ExternalError::Unavailable) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::Unavailable,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(ExternalError::ExecutionTimeout) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::Timeout,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(ExternalError::ProtocolDecode) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::ProtocolDecode,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(ExternalError::WrongRequestId) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::WrongRequestId,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(ExternalError::ModuleError) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::ModuleError,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(ExternalError::ResultTooLarge) => Response::plain_with_locale(
                self.locale(),
                external_command_text(
                    locale,
                    ExternalCommandText::ResultTooLarge,
                    &invocation.module_id,
                    None,
                ),
            ),
            Err(error) => {
                tracing::warn!(
                    event = "external_command_error",
                    module_id = %invocation.module_id,
                    command = %invocation.command_name,
                    error = %error,
                    "External command failed"
                );
                Response::plain_with_locale(
                    self.locale(),
                    external_command_text(
                        locale,
                        ExternalCommandText::GenericError,
                        &invocation.module_id,
                        Some(&error.to_string()),
                    ),
                )
            }
        };
        if result.is_err() {
            self.refresh_snapshot().await;
        }
        response
    }

    pub(crate) async fn execute(
        &mut self,
        client: &Client,
        action: &Action,
        message_id: i32,
        peer_id: PeerId,
        message_context: MessageExecutionContext<'_>,
    ) -> RuntimeExecution {
        self.recognized_commands = self.recognized_commands.saturating_add(1);
        let prefix = self.prefix().to_owned();
        if let Action::Start(request) = action {
            return self.execute_start(client, request, peer_id).await;
        }
        if let Action::Setup(request) = action {
            return self.execute_setup(client, request, peer_id).await;
        }
        if let Action::Info = action {
            return self.execute_info().await;
        }
        match action {
            Action::Language(request) => self.execute_language(request).await,
            Action::Ping => match telegram_ping(client, message_id).await {
                Ok(latency) => Response::plain_with_locale(
                    self.locale(),
                    ping_text(self.locale(), PingText::Success, &format_latency(latency)),
                ),
                Err(error) => {
                    log_ping_failure(action, message_id, &error);
                    Response::plain_with_locale(
                        self.locale(),
                        runtime_text(self.locale(), RuntimeText::PingFailed),
                    )
                }
            },
            Action::Stats => {
                let telegram = match telegram_ping(client, message_id).await {
                    Ok(latency) => format_latency(latency),
                    Err(error) => {
                        log_ping_failure(action, message_id, &error);
                        stats_text(self.locale(), StatsText::Unavailable).to_owned()
                    }
                };
                let proc_stats = read_proc_stats().await;
                log_unavailable_proc_stats(&proc_stats);
                Response::plain_with_locale(
                    self.locale(),
                    format_stats(
                        self.locale(),
                        &telegram,
                        self.started_at.elapsed(),
                        &proc_stats,
                        self.recognized_commands,
                    ),
                )
            }
            Action::Info => unreachable!("info actions return before response dispatch"),
            Action::Help(request) => {
                let rendered = render_with_external_locale(
                    request,
                    &prefix,
                    &self.aliases,
                    self.external_command_refs(),
                    self.external_descriptors(),
                    self.locale(),
                );
                if rendered.entity_fallback {
                    tracing::warn!(
                        event = "help_entity_fallback",
                        "Help formatting was unavailable"
                    );
                }
                rendered.response
            }
            Action::Fastfetch(arguments) => fastfetch_response(
                fastfetch::run(self.locale(), arguments, &self.fastfetch_profile_path).await,
                self.locale(),
                &prefix,
                &self.fastfetch_profile_path,
            ),
            Action::Alias(request) => self.execute_alias(request, &prefix).await,
            Action::Prefix(request) => self.execute_prefix(request).await,
            Action::Modules(request) => self.execute_modules(request, &prefix),
            Action::Lm(request) => self.execute_lm(client, message_context, request).await,
            Action::Reboot => return self.execute_reboot(message_context),
            Action::Setup(_) => unreachable!("setup actions return before response dispatch"),
            Action::Start(_) => unreachable!("start actions return before response dispatch"),
            Action::External(invocation) => self.execute_external(invocation).await,
        }
        .into()
    }

    /// Builds the `info` reply: a dynamic caption plus the static branding
    /// image when it is available. `media` is left `None` for a text-only card
    /// when the packaged image cannot be resolved.
    async fn execute_info(&mut self) -> RuntimeExecution {
        self.refresh_snapshot().await;
        let locale = self.locale();
        let prefix = self.prefix().to_owned();
        let owner = self
            .self_identity()
            .map(info::owner_label)
            .unwrap_or_else(|| info_text(locale, InfoText::Unknown).to_owned());
        let upstream = self.upstream_main_rev().await;
        let upstream = if upstream.is_empty() {
            info_text(locale, InfoText::Unavailable).to_owned()
        } else {
            info::short_commit(&upstream).to_owned()
        };
        let built_in_modules = crate::modules::modules().len();
        let total_modules = built_in_modules + self.external_descriptors().len();
        let active_modules = built_in_modules
            + self
                .external_snapshot
                .module_statuses
                .iter()
                .filter(|status| status.status == ExternalModuleRuntimeStatus::Running)
                .count();
        let host = info::deployment_label(std::env::var("LAVIS_HOST").ok().as_deref());
        let os = tokio::task::spawn_blocking(info::read_os_release_pretty_name)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| std::env::consts::OS.to_owned());
        let caption = render_info_text(
            locale,
            InfoCaptionData {
                owner: &owner,
                version: env!("CARGO_PKG_VERSION"),
                commit: info::short_commit(info::build_rev()),
                upstream: &upstream,
                prefix: &prefix,
                active_modules,
                total_modules,
                host,
                os: &os,
            },
        );
        let media = info::info_asset_path(
            std::env::var_os("LAVIS_INFO_IMAGE").as_deref(),
            env!("CARGO_MANIFEST_DIR"),
        );
        if media.is_none() {
            tracing::warn!(
                event = "info_image_unavailable",
                "Info image is missing; replying with a text-only card"
            );
        }
        RuntimeExecution {
            response: Response::plain_with_locale(locale, caption),
            media,
            provision: None,
            shutdown: None,
            post_edit: None,
            onboarding_page: false,
        }
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

    async fn execute_lm(
        &mut self,
        client: &Client,
        message_context: MessageExecutionContext<'_>,
        request: &LmRequest,
    ) -> Response {
        match self.module_approvals.list_pending() {
            Ok(pending) => tracing::debug!(
                event = "external_module_approvals_swept",
                pending = pending.len(),
                "Swept expired external module approvals"
            ),
            Err(error) => tracing::warn!(
                event = "external_module_approvals_sweep_failed",
                error = %error,
                "Could not sweep expired external module approvals"
            ),
        }
        if lm_request_mutates(request)
            && let Err(response) = self.lm_mutation_policy(message_context)
        {
            return response;
        }
        match request {
            LmRequest::Overview | LmRequest::List => self.render_lm_list().await,
            LmRequest::Info { id } => self.lm_info(id).await,
            LmRequest::Logs { id } => self.lm_logs(id).await,
            LmRequest::Doctor { id } => self.lm_doctor(id.as_deref()).await,
            LmRequest::Invalid => self.lm_invalid_usage_response(),
            LmRequest::Install => {
                self.inspect_module_install(client, message_context.message)
                    .await
            }
            LmRequest::Confirm { approval_id } => self.confirm_module_install(approval_id).await,
            LmRequest::Cancel { approval_id } => self.cancel_module_install(approval_id),
            LmRequest::Enable { id } => self.lm_set_enabled(id, true).await,
            LmRequest::Disable { id } => self.lm_set_enabled(id, false).await,
        }
    }

    async fn render_lm_list(&self) -> Response {
        let Some(config) = &self.module_control else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::Unavailable),
            );
        };
        let state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::StateUnavailable),
                );
            }
        };
        let list = match control::list_modules(&config.root, &config.declarative_state_path, &state)
        {
            Ok(list) => list,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::ListUnavailable),
                );
            }
        };
        let fresh_snapshot = match &self.external_manager {
            Some(handle) => handle.snapshot().await,
            None => self.external_snapshot.clone(),
        };
        let mut statuses = list
            .modules
            .iter()
            .map(|entry| match &entry.module {
                Some(module) => format!(
                    "• {}\n  {}: {}\n  {}: {}\n  {}: {}\n  {}: {}\n  {}: {}\n  {}: {}\n  {}: {}",
                    module.display_name,
                    lm_label(self.locale(), LmLabel::Id),
                    module.id,
                    lm_label(self.locale(), LmLabel::Version),
                    module.version,
                    lm_label(self.locale(), LmLabel::Status),
                    enabled_label(self.locale(), module.enabled),
                    lm_label(self.locale(), LmLabel::Source),
                    management_label(self.locale(), module.management),
                    lm_label(self.locale(), LmLabel::Runtime),
                    runtime_status_from_snapshot(self.locale(), &fresh_snapshot, &module.id),
                    lm_label(self.locale(), LmLabel::Author),
                    module.author,
                    lm_label(self.locale(), LmLabel::Commands),
                    module.commands.len()
                ),
                None => format!(
                    "• {}\n  {}: {}\n  {}: {}\n  {}: {}",
                    entry
                        .id
                        .as_deref()
                        .unwrap_or(lm_label(self.locale(), LmLabel::InvalidId)),
                    lm_label(self.locale(), LmLabel::Diagnostic),
                    diagnostic_label(self.locale(), entry.diagnostic.as_ref()),
                    lm_label(self.locale(), LmLabel::Status),
                    enabled_label(self.locale(), entry.enabled),
                    lm_label(self.locale(), LmLabel::Source),
                    management_label(self.locale(), entry.management)
                ),
            })
            .collect::<Vec<_>>();
        for id in state.enabled_ids() {
            if !list
                .modules
                .iter()
                .any(|entry| entry.id.as_deref() == Some(id))
            {
                statuses.push(format!(
                    "• {id}\n  {}: {}",
                    lm_label(self.locale(), LmLabel::Status),
                    lm_label(self.locale(), LmLabel::MissingCatalog)
                ));
            }
        }
        if statuses.is_empty() {
            Response::plain_with_locale(
                self.locale(),
                lm_format(self.locale(), LmText::Empty, "", self.prefix()),
            )
        } else {
            Response::plain_with_locale(
                self.locale(),
                format!(
                    "{}\n\n{}",
                    lm_text(self.locale(), LmText::ListHeading),
                    statuses.join("\n\n")
                ),
            )
        }
    }

    fn lm_invalid_usage_response(&self) -> Response {
        Response::plain_with_locale(self.locale(), lm_usage(self.locale(), self.prefix()))
    }

    fn lm_mutation_policy(&self, context: MessageExecutionContext<'_>) -> Result<(), Response> {
        self.authorize_sensitive_command(
            SensitiveCommandPolicy::ModuleMutation,
            context,
            self.module_control
                .as_ref()
                .map(|control| control.saved_messages_peer),
        )
    }

    async fn lm_logs(&self, id: &str) -> Response {
        let Some(handle) = &self.external_manager else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::RuntimeUnavailable),
            );
        };
        match handle.diagnostic_text(id).await {
            Some(diagnostic) => Response::plain_with_locale(
                self.locale(),
                lm_format(self.locale(), LmText::LastModuleError, id, &diagnostic),
            ),
            None => Response::plain_with_locale(
                self.locale(),
                lm_format(self.locale(), LmText::NoRuntimeError, id, ""),
            ),
        }
    }

    async fn lm_doctor(&self, id: Option<&str>) -> Response {
        let Some(config) = &self.module_control else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::Unavailable),
            );
        };
        let state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::StateUnavailable),
                );
            }
        };
        let list = match control::list_modules(&config.root, &config.declarative_state_path, &state)
        {
            Ok(list) => list,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::ListUnavailable),
                );
            }
        };
        let mut lines = Vec::new();
        for entry in &list.modules {
            if let Some(module) = &entry.module {
                if let Some(target) = id
                    && module.id != target
                {
                    continue;
                }
                let runtime = fresh_runtime_status(
                    self.locale(),
                    self.external_manager.as_ref(),
                    &self.external_snapshot,
                    &module.id,
                )
                .await;
                let diagnostic = if let Some(handle) = &self.external_manager {
                    handle.diagnostic_summary(&module.id).await
                } else {
                    None
                };
                lines.push(render_lm_doctor_module(
                    self.locale(),
                    &module.display_name,
                    &module.id,
                    enabled_label(self.locale(), module.enabled),
                    &runtime,
                    management_label(self.locale(), module.management),
                    diagnostic.as_deref(),
                ));
            }
        }
        for enabled in state.enabled_ids() {
            let listed = list
                .modules
                .iter()
                .any(|entry| entry.id.as_deref() == Some(enabled));
            if !listed
                && let Some(target) = id
                && enabled != target
            {
                continue;
            }
            lines.push(render_lm_doctor_missing_catalog(self.locale(), enabled));
        }
        let Some(target) = id else {
            return Response::plain_with_locale(
                self.locale(),
                render_lm_doctor_report(self.locale(), None, &lines),
            );
        };
        if lines.is_empty() {
            return Response::plain_with_locale(
                self.locale(),
                lm_format(self.locale(), LmText::NotFound, target, ""),
            );
        }
        Response::plain_with_locale(
            self.locale(),
            render_lm_doctor_report(self.locale(), Some(target), &lines),
        )
    }

    async fn lm_info(&self, id: &str) -> Response {
        let Some(config) = &self.module_control else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::Unavailable),
            );
        };
        let state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::StateUnavailable),
                );
            }
        };
        let diagnostic = if let Some(handle) = &self.external_manager {
            handle.diagnostic_text(id).await
        } else {
            None
        };
        match control::module_info(&config.root, &config.declarative_state_path, &state, id) {
            Ok(module) => {
                let locale = self.locale();
                let capabilities = capabilities_label(locale, &module.capabilities);
                let commands = commands_label(locale, &module.commands);
                let runtime = fresh_runtime_status(
                    locale,
                    self.external_manager.as_ref(),
                    &self.external_snapshot,
                    &module.id,
                )
                .await;
                Response::plain_with_locale(
                    self.locale(),
                    render_lm_info(
                        locale,
                        LmInfoResponse {
                            display_name: &module.display_name,
                            id: &module.id,
                            version: &module.version,
                            author: &module.author,
                            enabled: enabled_label(locale, module.enabled),
                            management: management_label(locale, module.management),
                            entrypoint: &module.entrypoint,
                            protocol_version: module.protocol_version,
                            capabilities: &capabilities,
                            commands: &commands,
                            runtime: &runtime,
                            diagnostic: diagnostic.as_deref(),
                        },
                    ),
                )
            }
            Err(control::ModuleControlError::InvalidInstalledModule) => {
                Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::InvalidManifest),
                )
            }
            Err(control::ModuleControlError::ModuleNotInstalled) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::NotInstalled),
            ),
            Err(_) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::NotInstalledOrUnavailable),
            ),
        }
    }

    async fn lm_set_enabled(&self, id: &str, enabled: bool) -> Response {
        let Some(config) = &self.module_control else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::Unavailable),
            );
        };
        let mut state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::StateUnavailable),
                );
            }
        };
        let result = if enabled {
            control::enable_module(&config.root, &config.declarative_state_path, &mut state, id)
                .await
        } else {
            control::disable_module(&config.root, &config.declarative_state_path, &mut state, id)
                .await
        };
        match result {
            Ok(operation) if operation.changed => Response::plain_with_locale(
                self.locale(),
                lm_state_text(
                    self.locale(),
                    LmText::StateChanged,
                    &operation.module.display_name,
                    enabled_label(self.locale(), enabled),
                    self.prefix(),
                ),
            ),
            Ok(operation) => Response::plain_with_locale(
                self.locale(),
                lm_state_text(
                    self.locale(),
                    LmText::StateUnchanged,
                    &operation.module.display_name,
                    enabled_label(self.locale(), enabled),
                    self.prefix(),
                ),
            ),
            Err(control::ModuleControlError::DeclarativelyManaged) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::Declarative),
            ),
            Err(_) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::StateChangeFailed),
            ),
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

    fn authorize_sensitive_command(
        &self,
        policy: SensitiveCommandPolicy,
        context: MessageExecutionContext<'_>,
        saved_messages_peer: Option<PeerId>,
    ) -> Result<(), Response> {
        authorize_sensitive_message(
            policy,
            context.edited,
            context.authored_by_self,
            context.message.peer_id(),
            context.message.id(),
            saved_messages_peer,
        )
        .map_err(|reason| {
            let text = match policy {
                SensitiveCommandPolicy::ModuleMutation => {
                    sensitive_text(self.locale(), SensitiveText::ModuleMutationDenied)
                }
                SensitiveCommandPolicy::Reboot => reason.response(self.locale(), policy),
            };
            Response::plain_with_locale(self.locale(), text)
        })
    }

    async fn inspect_module_install(&mut self, client: &Client, message: &Message) -> Response {
        let Some(installation) = &self.module_installation else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::InstallUnavailable),
            );
        };
        let acquired = match ModuleSourceAcquirer::new(
            client,
            installation.saved_messages_peer,
            AcquisitionLimits::default(),
        )
        .acquire(message)
        .await
        {
            Ok(acquired) => acquired,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::AttachPackage),
                );
            }
        };
        let config = InspectionConfig {
            staging_root: installation.staging_root.clone(),
            limits: InspectionLimits::default(),
        };
        let now = SystemTime::now();
        let pending = match ModuleInspector::new(&config, OsRandom).inspect(
            acquired,
            now,
            now + DEFAULT_APPROVAL_TTL,
        ) {
            Ok(pending) => pending,
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::UnsafePackage),
                );
            }
        };
        let prefix = self.prefix().to_owned();
        let locale = self.locale();
        match self.module_approvals.issue(pending) {
            Ok((id, _)) => match self.module_approvals.get(id) {
                Ok(plan) => Response::plain_with_locale(
                    locale,
                    render_install_plan(locale, plan, id, &prefix),
                ),
                Err(error) => {
                    tracing::warn!(
                        event = "external_module_approval_plan_unavailable",
                        error = %error,
                        "Issued external module approval could not be read back"
                    );
                    Response::plain_with_locale(locale, lm_text(locale, LmText::PlanUnavailable))
                }
            },
            Err(_) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::PlanUnavailable),
            ),
        }
    }

    async fn confirm_module_install(&mut self, supplied: &crate::commands::ApprovalId) -> Response {
        let Ok(id) = ApprovalId::parse(supplied.as_str()) else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::ApprovalInvalid),
            );
        };
        let Some(installation) = &self.module_installation else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::InstallUnavailable),
            );
        };
        let module_id = match self.module_approvals.get(id) {
            Ok(plan) => plan.module_id.clone(),
            Err(ApprovalError::Unavailable | ApprovalError::InvalidId) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::ApprovalInvalid),
                );
            }
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::PlanUnavailable),
                );
            }
        };
        if let Some(handle) = &self.external_manager {
            let manager = handle.lock().await;
            if manager.descriptor_by_id(&module_id).is_some() {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_format(self.locale(), LmText::AlreadyRegistered, &module_id, ""),
                );
            }
        }
        let pending = match self.module_approvals.redeem(id) {
            Ok(pending) => pending,
            Err(ApprovalError::Unavailable | ApprovalError::InvalidId) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::ApprovalInvalid),
                );
            }
            Err(_) => {
                return Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::PlanUnavailable),
                );
            }
        };
        let Some(wrapper) = pending.stage.take_wrapper() else {
            return Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::VerifiedPackageUnavailable),
            );
        };
        let installed = match crate::external_modules::installer::install_staged_module(
            &wrapper,
            &installation.root,
            &module_id,
        ) {
            Ok(installed) => installed,
            Err(error) => {
                if let Err(cleanup) =
                    crate::external_modules::installer::cleanup_redeemed_stage(&wrapper)
                {
                    tracing::warn!(
                        event = "external_module_redeemed_stage_cleanup_failed",
                        wrapper = %cleanup.wrapper.display(),
                        ?cleanup.kind,
                        "Could not remove redeemed external module staging"
                    );
                }
                return match error {
                    crate::external_modules::installer::InstallError::TargetCleanup(_) => {
                        Response::plain_with_locale(self.locale(), lm_text(self.locale(), LmText::InstallRollbackFailed))
                    }
                    crate::external_modules::installer::InstallError::PostInstallValidationFailed { .. } => {
                        Response::plain_with_locale(self.locale(), lm_text(self.locale(), LmText::InstallValidationFailed))
                    }
                    _ => Response::plain_with_locale(self.locale(), lm_text(self.locale(), LmText::InstallFailed)),
                };
            }
        };
        if let Some(handle) = &self.external_manager {
            let registered = {
                let mut manager = handle.lock().await;
                manager.register_installed_descriptor(installed.descriptor)
            };
            if !registered {
                tracing::warn!(
                    event = "external_module_descriptor_registration_duplicate",
                    module_id = %module_id,
                    "Installed external module already has a registered descriptor"
                );
                self.refresh_snapshot().await;
                return Response::plain_with_locale(
                    self.locale(),
                    lm_format(self.locale(), LmText::RegistrationConflict, &module_id, ""),
                );
            }
        }
        self.refresh_snapshot().await;
        Response::plain_with_locale(
            self.locale(),
            lm_format(self.locale(), LmText::InstalledDisabled, &module_id, ""),
        )
    }

    fn cancel_module_install(&mut self, supplied: &crate::commands::ApprovalId) -> Response {
        match ApprovalId::parse(supplied.as_str()) {
            Ok(id) => match self.module_approvals.revoke(id) {
                Ok(true) => Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::Cancelled),
                ),
                Ok(false) => Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::ApprovalInvalid),
                ),
                Err(_) => Response::plain_with_locale(
                    self.locale(),
                    lm_text(self.locale(), LmText::PlanUnavailable),
                ),
            },
            Err(_) => Response::plain_with_locale(
                self.locale(),
                lm_text(self.locale(), LmText::ApprovalInvalid),
            ),
        }
    }

    async fn execute_setup(
        &mut self,
        client: &Client,
        request: &SetupRequest,
        peer: PeerId,
    ) -> RuntimeExecution {
        let locale = self.locale();
        let Some(setup) = &mut self.setup else {
            return Response::plain_with_locale(
                self.locale(),
                setup_text(locale, SetupText::Unavailable),
            )
            .into();
        };
        if peer != setup.saved_messages_peer {
            return Response::plain_with_locale(
                self.locale(),
                setup_text(locale, SetupText::SavedMessagesOnly),
            )
            .into();
        }
        let execution = setup.handle_command(client, request, locale).await;
        if setup.botfather_peer.is_some() {
            self.external_projection_permitted = true;
        }
        execution
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SensitiveCommandPolicy {
    ModuleMutation,
    Reboot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SensitiveCommandDenial {
    Edited,
    NotSelfAuthored,
    InvalidMessageId,
    NotSavedMessages,
}

impl SensitiveCommandDenial {
    fn response(self, locale: Locale, policy: SensitiveCommandPolicy) -> &'static str {
        match (policy, self) {
            (SensitiveCommandPolicy::ModuleMutation, _) => {
                sensitive_text(locale, SensitiveText::ModuleMutationDenied)
            }
            (SensitiveCommandPolicy::Reboot, _) => {
                sensitive_text(locale, SensitiveText::RebootDenied)
            }
        }
    }
}

fn authorize_sensitive_message(
    policy: SensitiveCommandPolicy,
    edited: bool,
    authored_by_self: bool,
    peer_id: PeerId,
    message_id: i32,
    saved_messages_peer: Option<PeerId>,
) -> Result<(), SensitiveCommandDenial> {
    if edited {
        return Err(SensitiveCommandDenial::Edited);
    }
    if !authored_by_self {
        return Err(SensitiveCommandDenial::NotSelfAuthored);
    }
    if message_id <= 0 {
        return Err(SensitiveCommandDenial::InvalidMessageId);
    }
    if policy == SensitiveCommandPolicy::ModuleMutation && saved_messages_peer != Some(peer_id) {
        return Err(SensitiveCommandDenial::NotSavedMessages);
    }
    Ok(())
}

fn lm_request_mutates(request: &LmRequest) -> bool {
    matches!(
        request,
        LmRequest::Install
            | LmRequest::Confirm { .. }
            | LmRequest::Cancel { .. }
            | LmRequest::Enable { .. }
            | LmRequest::Disable { .. }
    )
}

fn enabled_label(locale: Locale, enabled: bool) -> &'static str {
    lm_label(
        locale,
        if enabled {
            LmLabel::Enabled
        } else {
            LmLabel::Disabled
        },
    )
}

fn management_label(locale: Locale, management: control::ModuleManagement) -> &'static str {
    match management {
        control::ModuleManagement::Manual => lm_label(locale, LmLabel::Manual),
        control::ModuleManagement::DeclarativeNixOs => lm_label(locale, LmLabel::Declarative),
    }
}

fn diagnostic_label(
    locale: Locale,
    diagnostic: Option<&control::ModuleDiagnostic>,
) -> &'static str {
    match diagnostic {
        Some(control::ModuleDiagnostic::InvalidModuleId) => lm_label(locale, LmLabel::InvalidId),
        Some(control::ModuleDiagnostic::InvalidManifest) => {
            lm_label(locale, LmLabel::InvalidManifest)
        }
        None => lm_label(locale, LmLabel::None),
    }
}

fn runtime_status_from_snapshot(
    locale: Locale,
    snapshot: &ExternalRuntimeSnapshot,
    id: &str,
) -> String {
    snapshot
        .module_statuses
        .iter()
        .find(|status| status.id == id)
        .map(|status| lm_runtime_status(locale, status.status).to_owned())
        .unwrap_or_else(|| lm_label(locale, LmLabel::NotRunning).to_owned())
}

async fn fresh_runtime_status(
    locale: Locale,
    handle: Option<&ExternalManagerHandle>,
    cached: &ExternalRuntimeSnapshot,
    id: &str,
) -> String {
    match handle {
        Some(handle) => runtime_status_from_snapshot(locale, &handle.snapshot().await, id),
        None => runtime_status_from_snapshot(locale, cached, id),
    }
}

fn capabilities_label(locale: Locale, capabilities: &[ExternalCapability]) -> String {
    if capabilities.is_empty() {
        lm_label(locale, LmLabel::None).to_owned()
    } else {
        capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn commands_label(
    locale: Locale,
    commands: &[crate::external_modules::manifest::ExternalCommandDescriptor],
) -> String {
    if commands.is_empty() {
        lm_label(locale, LmLabel::None).to_owned()
    } else {
        commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn lm_usage(locale: Locale, prefix: &str) -> String {
    lm_text(locale, LmText::Usage).replace("{prefix}", prefix)
}

fn bounded_list(locale: Locale, values: &[String]) -> String {
    const MAX_ITEMS: usize = 8;
    const MAX_VALUE_CHARS: usize = 96;
    if values.is_empty() {
        return lm_label(locale, LmLabel::None).to_owned();
    }
    let mut rendered = values
        .iter()
        .take(MAX_ITEMS)
        .map(|value| value.chars().take(MAX_VALUE_CHARS).collect::<String>())
        .collect::<Vec<_>>();
    if values.len() > MAX_ITEMS {
        let remaining = values.len() - MAX_ITEMS;
        rendered.push(match locale {
            Locale::English => format!("+{remaining}"),
            Locale::Russian => format!("ещё {remaining}"),
        });
    }
    rendered.join(", ")
}

fn render_install_plan(
    locale: Locale,
    plan: &crate::external_modules::source_inspection::ModuleInstallPlan,
    approval_id: ApprovalId,
    prefix: &str,
) -> String {
    let source = match &plan.source_identity {
        crate::external_modules::source_inspection::SourceIdentity::Archive => {
            lm_label(locale, LmLabel::Archive).to_owned()
        }
        crate::external_modules::source_inspection::SourceIdentity::PinnedRepository(
            repository,
        ) => {
            format!(
                "{} {} @ {}",
                lm_label(locale, LmLabel::Repository),
                repository.repository(),
                repository.revision()
            )
        }
    };
    let capabilities = bounded_list(locale, &plan.capabilities);
    let subscriptions = bounded_list(locale, &plan.subscriptions);
    let methods = bounded_list(locale, &plan.telegram_methods);
    let actions = bounded_list(locale, &plan.actions);
    let warnings = bounded_list(
        locale,
        &plan
            .warnings
            .iter()
            .map(|warning| inspection_warning_text(locale, warning).to_owned())
            .collect::<Vec<_>>(),
    );
    render_lm_install_plan(
        locale,
        LmInstallPlanText {
            source: &source,
            module: &plan.module_id,
            version: &plan.module_version,
            protocol: plan.protocol_version,
            entrypoint: &plan.entrypoint,
            default_command: plan
                .default_command
                .as_deref()
                .unwrap_or(lm_label(locale, LmLabel::None)),
            sha256: &plan.archive_digest.as_hex(),
            fingerprint: &plan.fingerprint,
            archive_bytes: plan.archive.archive_bytes,
            file_count: plan.archive.file_count as usize,
            compressed_bytes: plan.archive.compressed_bytes,
            expanded_bytes: plan.archive.expanded_bytes,
            capabilities: &capabilities,
            subscriptions: &subscriptions,
            methods: &methods,
            actions: &actions,
            warnings: &warnings,
            approval_id: &approval_id.to_string(),
            prefix,
        },
    )
}

impl SetupCoordinator {
    fn is_active(&self) -> bool {
        !matches!(self.phase, SetupPhase::Idle)
    }

    async fn handle_command(
        &mut self,
        client: &Client,
        request: &SetupRequest,
        locale: Locale,
    ) -> RuntimeExecution {
        if matches!(
            request,
            SetupRequest::Start | SetupRequest::Auto | SetupRequest::Username(_)
        ) {
            if self.is_active() {
                return Response::plain(setup_text(locale, SetupText::AlreadyActive)).into();
            }
            if self.has_created_bot().await {
                return Response::plain(setup_text(locale, SetupText::ExistingBot)).into();
            }
        }
        if matches!(request, SetupRequest::Repair) {
            return self.repair(client, locale).await;
        }
        match request {
            SetupRequest::Status => self.status(locale).await,
            SetupRequest::Cancel => {
                if !self.is_active() {
                    return Response::plain(setup_text(locale, SetupText::NoActiveSetup)).into();
                }
                self.phase = SetupPhase::Idle;
                Response::plain(setup_text(locale, SetupText::Cancelled))
            }
            SetupRequest::Start => {
                self.phase = SetupPhase::AwaitingUsername {
                    automatic: false,
                    deadline: Instant::now() + SETUP_STAGE_TIMEOUT,
                };
                Response::plain(setup_text(locale, SetupText::UsernamePrompt))
            }
            SetupRequest::Auto => match setup::generate_candidate() {
                Ok(username) => self.confirm_or_start(username, true, 1, locale).await,
                Err(_) => Response::plain(setup_text(locale, SetupText::UsernameGenerationFailed)),
            },
            SetupRequest::Username(value) => match setup::validate_username(value) {
                Ok(username) => self.confirm_or_start(username, false, 1, locale).await,
                Err(_) => Response::plain(setup_text(locale, SetupText::UsernameInvalid)),
            },
            SetupRequest::Repair => unreachable!("repair returns a provisioning request"),
            SetupRequest::Invalid => Response::plain(setup_text(locale, SetupText::Usage)),
        }
        .into()
    }

    async fn confirm_or_start(
        &mut self,
        username: UsernameCandidate,
        automatic: bool,
        attempts: u8,
        locale: Locale,
    ) -> Response {
        self.phase = SetupPhase::AwaitingConfirmation {
            username: username.clone(),
            automatic,
            attempts,
            deadline: Instant::now() + SETUP_STAGE_TIMEOUT,
        };
        let plan = setup_text(locale, SetupText::Plan)
            .replace("{username}", username.display())
            .replace("{display_name}", crate::setup_telegram::DISPLAY_NAME);
        Response::plain(plan)
    }

    async fn handle_input(&mut self, client: &Client, text: &str, locale: Locale) -> Response {
        if matches!(
            setup::parse_confirmation(text),
            Some(setup::Confirmation::Cancelled)
        ) {
            self.phase = SetupPhase::Idle;
            return Response::plain(setup_text(locale, SetupText::Cancelled));
        }
        match &self.phase {
            SetupPhase::AwaitingUsername { .. } => self.handle_username_input(text, locale).await,
            SetupPhase::AwaitingConfirmation {
                username,
                automatic,
                attempts,
                ..
            } => {
                if matches!(
                    setup::parse_confirmation(text),
                    Some(setup::Confirmation::Confirmed)
                ) {
                    self.start_flow(client, username.clone(), *automatic, *attempts, locale)
                        .await
                } else {
                    Response::plain(setup_text(locale, SetupText::ConfirmOrCancel))
                }
            }
            _ => Response::plain(setup_text(locale, SetupText::WaitingBotFather)),
        }
    }

    async fn handle_username_input(&mut self, text: &str, locale: Locale) -> Response {
        let automatic = match &self.phase {
            SetupPhase::AwaitingUsername { automatic, .. } => *automatic,
            _ => return Response::plain(setup_text(locale, SetupText::WaitingBotFather)),
        };
        let generated = matches!(text.trim().to_ascii_lowercase().as_str(), "-" | "auto");
        let username = match generated {
            true => setup::generate_candidate()
                .map_err(|_| crate::setup::UsernameError::InvalidCharactersOrLength),
            false => setup::validate_username(text.trim()),
        };
        match username {
            Ok(username) => {
                self.confirm_or_start(username, automatic || generated, 1, locale)
                    .await
            }
            Err(_) => Response::plain(setup_text(locale, SetupText::UsernameInvalid)),
        }
    }

    async fn start_flow(
        &mut self,
        client: &Client,
        username: UsernameCandidate,
        automatic: bool,
        attempts: u8,
        locale: Locale,
    ) -> Response {
        let Ok((transport, peer)) = GrammersTelegramSetup::resolve(client).await else {
            return Response::plain(setup_text(locale, SetupText::BotFatherUnavailable));
        };
        let mut flow =
            CompanionSetup::new(username, self.state_path.clone(), self.token_path.clone());
        if flow.start(&transport).await.is_err() {
            return Response::plain(setup_text(locale, SetupText::BotFatherStartFailed));
        }
        self.botfather_peer = Some(peer);
        self.phase = SetupPhase::Running {
            flow,
            transport,
            automatic,
            attempts,
            deadline: Instant::now() + SETUP_STAGE_TIMEOUT,
        };
        Response::plain(setup_text(locale, SetupText::Started))
    }

    async fn handle_botfather_reply(
        &mut self,
        client: &Client,
        text: &str,
        locale: Locale,
    ) -> BotFatherOutcome {
        let SetupPhase::Running {
            flow,
            transport,
            automatic,
            attempts,
            deadline,
        } = &mut self.phase
        else {
            return BotFatherOutcome {
                response: None,
                provision: None,
            };
        };
        let api = match HttpBotApi::new() {
            Ok(api) => api,
            Err(_) => {
                self.phase = SetupPhase::Idle;
                return BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotCheckUnavailable,
                    ))),
                    provision: None,
                };
            }
        };
        match flow.on_botfather_reply(text, transport, &api).await {
            Ok(BotFatherProgress::Pending) => {
                *deadline = Instant::now() + SETUP_STAGE_TIMEOUT;
                BotFatherOutcome {
                    response: None,
                    provision: None,
                }
            }
            Ok(BotFatherProgress::ProvisionReady) => {
                let request = flow.provision_request(client.clone());
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: None,
                    provision: Some(request),
                }
            }
            Ok(BotFatherProgress::UsernameOccupied) if *automatic && *attempts < 10 => {
                let next_attempt = *attempts + 1;
                self.phase = SetupPhase::Idle;
                match setup::generate_candidate() {
                    Ok(username) => {
                        let response = self
                            .start_flow(client, username, true, next_attempt, locale)
                            .await;
                        BotFatherOutcome {
                            response: Some(response),
                            provision: None,
                        }
                    }
                    Err(_) => BotFatherOutcome {
                        response: Some(Response::plain(setup_text(
                            locale,
                            SetupText::UsernameGenerationFailed,
                        ))),
                        provision: None,
                    },
                }
            }
            Ok(BotFatherProgress::UsernameOccupied | BotFatherProgress::UsernameInvalid) => {
                self.phase = SetupPhase::AwaitingUsername {
                    automatic: false,
                    deadline: Instant::now() + SETUP_STAGE_TIMEOUT,
                };
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotUsernameRejected,
                    ))),
                    provision: None,
                }
            }
            Ok(BotFatherProgress::LimitReached) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotLimitReached,
                    ))),
                    provision: None,
                }
            }
            Ok(BotFatherProgress::FloodWait) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotRetryLater,
                    ))),
                    provision: None,
                }
            }
            Ok(BotFatherProgress::Unexpected) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotUnexpected,
                    ))),
                    provision: None,
                }
            }
            Err(crate::setup_telegram::SetupTelegramError::Storage) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotStorageFailed,
                    ))),
                    provision: None,
                }
            }
            Err(crate::setup_telegram::SetupTelegramError::Timeout) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(locale, SetupText::BotTimedOut))),
                    provision: None,
                }
            }
            Err(_) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(setup_text(
                        locale,
                        SetupText::BotVerificationFailed,
                    ))),
                    provision: None,
                }
            }
        }
    }

    async fn status(&self, locale: Locale) -> Response {
        let state_path = self.state_path.clone();
        let token_path = self.token_path.clone();
        match tokio::task::spawn_blocking(move || {
            SetupStore::new(state_path, token_path).load_state()
        })
        .await
        {
            Ok(Ok(state)) => setup_status_response(
                locale,
                &state.status,
                state.identities.bot_username.as_deref(),
            ),
            Ok(Err(crate::error::SetupStoreError::NotFound)) => {
                Response::plain(setup_text(locale, SetupText::StatusIdle))
            }
            Ok(Err(_)) | Err(_) => {
                Response::plain(setup_text(locale, SetupText::StatusUnavailable))
            }
        }
    }

    async fn has_created_bot(&self) -> bool {
        let state_path = self.state_path.clone();
        let token_path = self.token_path.clone();
        matches!(
            tokio::task::spawn_blocking(move || SetupStore::new(state_path, token_path).load_state()).await,
            Ok(Ok(state))
                if state.identities.bot_username.is_some()
                    && state.identities.bot_user_id.is_some()
        )
    }

    async fn repair(&self, client: &Client, locale: Locale) -> RuntimeExecution {
        let api = match HttpBotApi::new() {
            Ok(api) => api,
            Err(_) => {
                return Response::plain(setup_text(locale, SetupText::RepairCheckUnavailable))
                    .into();
            }
        };
        self.repair_with_api(client, &api, locale).await
    }

    async fn repair_with_api(
        &self,
        client: &Client,
        bot_api: &impl BotApi,
        locale: Locale,
    ) -> RuntimeExecution {
        match self.repair_preflight(bot_api, locale).await {
            Ok(username) => RuntimeExecution {
                response: Response::plain(setup_text(locale, SetupText::RepairStarted)),
                provision: Some(ProvisionRequest::new(
                    client.clone(),
                    self.state_path.clone(),
                    self.token_path.clone(),
                    username,
                )),
                shutdown: None,
                post_edit: None,
                onboarding_page: false,
                media: None,
            },
            Err(response) => response.into(),
        }
    }

    async fn repair_preflight(
        &self,
        bot_api: &impl BotApi,
        locale: Locale,
    ) -> Result<String, Response> {
        let state_path = self.state_path.clone();
        let token_path = self.token_path.clone();
        let loaded = tokio::task::spawn_blocking({
            let state_path = state_path.clone();
            let token_path = token_path.clone();
            move || {
                let store = SetupStore::new(state_path, token_path);
                Ok::<_, crate::error::SetupStoreError>((store.load_state()?, store.load_token()?))
            }
        })
        .await;
        let (state, token) = match loaded {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(crate::error::SetupStoreError::NotFound)) => {
                return Err(Response::plain(setup_text(locale, SetupText::RepairNoData)));
            }
            Ok(Err(_)) | Err(_) => {
                return Err(Response::plain(setup_text(
                    locale,
                    SetupText::RepairDataUnsafe,
                )));
            }
        };
        let username = match state.identities.bot_username.as_deref() {
            Some(username) if setup::validate_username(username).is_ok() => username.to_owned(),
            _ => {
                return Err(Response::plain(setup_text(
                    locale,
                    SetupText::RepairIdentityUnsafe,
                )));
            }
        };
        let persisted_bot_id = state.identities.bot_user_id;
        let identity =
            match tokio::time::timeout(Duration::from_secs(25), bot_api.get_me(&token)).await {
                Ok(Ok(identity)) => identity,
                Ok(Err(_)) | Err(_) => {
                    return Err(Response::plain(setup_text(
                        locale,
                        SetupText::RepairTokenUnsafe,
                    )));
                }
            };
        if !identity.username.eq_ignore_ascii_case(&username) {
            return Err(Response::plain(setup_text(
                locale,
                SetupText::RepairTokenMismatch,
            )));
        }
        if let Some(bot_id) = persisted_bot_id {
            if identity.id != bot_id {
                return Err(Response::plain(setup_text(
                    locale,
                    SetupText::RepairTokenMismatch,
                )));
            }
        } else {
            let state_path = self.state_path.clone();
            let token_path = self.token_path.clone();
            let verified_username = identity.username;
            let verified_id = identity.id;
            let expected_username = username.clone();
            let persisted = tokio::task::spawn_blocking(move || {
                let mut store = SetupStore::new(state_path, token_path);
                let mut current = store.load_state()?;
                if current.identities.bot_username.as_deref() != Some(expected_username.as_str())
                    || current.identities.bot_user_id.is_some()
                {
                    return Err(crate::error::SetupStoreError::Read);
                }
                current.identities.bot_username = Some(verified_username);
                current.identities.bot_user_id = Some(verified_id);
                store.save_state(&current)
            })
            .await;
            if !matches!(persisted, Ok(Ok(()))) {
                return Err(Response::plain(setup_text(
                    locale,
                    SetupText::RepairIdentitySaveFailed,
                )));
            }
        }
        Ok(username)
    }
}

fn setup_status_label(locale: Locale, status: &str) -> &'static str {
    let key = match status {
        "idle" => SetupText::StatusIdleValue,
        "bot_validated" => SetupText::StatusBotValidated,
        "complete" => SetupText::StatusComplete,
        "completed_without_folder_capacity" => SetupText::StatusCompletedWithoutFolderCapacity,
        "completed_without_folder_name_conflict" => {
            SetupText::StatusCompletedWithoutFolderNameConflict
        }
        "companion_and_community_configured" => SetupText::StatusCompanionAndCommunityConfigured,
        "companion_configured_community_pending" => {
            SetupText::StatusCompanionConfiguredCommunityPending
        }
        _ => SetupText::StatusUnknown,
    };
    setup_text(locale, key)
}

fn setup_status_response(locale: Locale, status: &str, bot: Option<&str>) -> Response {
    Response::plain_with_locale(
        locale,
        setup_text(locale, SetupText::Status)
            .replace("{status}", setup_status_label(locale, status))
            .replace(
                "{bot}",
                bot.unwrap_or(setup_text(locale, SetupText::NotConfigured)),
            ),
    )
}

fn fastfetch_response(
    result: FastfetchResult,
    locale: Locale,
    prefix: &str,
    profile_path: &std::path::Path,
) -> Response {
    match result {
        FastfetchResult::Success(response) => response,
        FastfetchResult::Empty => fastfetch_failure(locale, FastfetchText::Empty, prefix),
        FastfetchResult::TimedOut => fastfetch_failure(locale, FastfetchText::TimedOut, prefix),
        FastfetchResult::Unavailable => {
            fastfetch_failure(locale, FastfetchText::Unavailable, prefix)
        }
        FastfetchResult::NonZero { code, .. } => Response::plain_with_locale(
            locale,
            fastfetch_text(locale, FastfetchText::NonZero)
                .replace("{code}", &code.to_string())
                .replace("{prefix}", prefix),
        ),
        FastfetchResult::UnexpectedStatus => {
            fastfetch_failure(locale, FastfetchText::UnexpectedStatus, prefix)
        }
        FastfetchResult::InvalidArguments(error) => {
            fastfetch_failure(locale, fastfetch_input_text(error), prefix)
        }
        FastfetchResult::ProfileError(error) => Response::plain_with_locale(
            locale,
            fastfetch_text(locale, fastfetch_profile_error_text(error))
                .replace("{path}", &format!("{profile_path:?}"))
                .replace("{prefix}", prefix),
        ),
    }
}

fn fastfetch_profile_error_text(error: FastfetchProfileError) -> FastfetchText {
    match error {
        FastfetchProfileError::NotReadable => FastfetchText::ProfileNotReadable,
        FastfetchProfileError::Malformed => FastfetchText::ProfileMalformed,
        FastfetchProfileError::UnsupportedVersion => FastfetchText::ProfileUnsupportedVersion,
        FastfetchProfileError::TooLarge => FastfetchText::ProfileTooLarge,
        FastfetchProfileError::UnsafePath => FastfetchText::ProfileUnsafePath,
        FastfetchProfileError::InvalidLogo => FastfetchText::ProfileInvalidLogo,
        FastfetchProfileError::InvalidStructure => FastfetchText::ProfileInvalidStructure,
        FastfetchProfileError::InvalidSeparator => FastfetchText::ProfileInvalidSeparator,
        FastfetchProfileError::InvalidLogoPadding => FastfetchText::ProfileInvalidLogoPadding,
    }
}

fn fastfetch_failure(locale: Locale, key: FastfetchText, prefix: &str) -> Response {
    Response::plain_with_locale(
        locale,
        fastfetch_text(locale, key).replace("{prefix}", prefix),
    )
}

fn fastfetch_input_text(error: FastfetchInputError) -> FastfetchText {
    match error {
        FastfetchInputError::Tokenization => FastfetchText::InputTokenization,
        FastfetchInputError::UnsupportedOption => FastfetchText::InputUnsupportedOption,
        FastfetchInputError::MissingValue => FastfetchText::InputMissingValue,
        FastfetchInputError::DuplicateOption => FastfetchText::InputDuplicateOption,
        FastfetchInputError::InvalidLogo => FastfetchText::InputInvalidLogo,
        FastfetchInputError::InvalidStructure => FastfetchText::InputInvalidStructure,
        FastfetchInputError::InvalidSeparator => FastfetchText::InputInvalidSeparator,
        FastfetchInputError::InvalidLogoPadding => FastfetchText::InputInvalidLogoPadding,
    }
}

async fn telegram_ping(
    client: &Client,
    message_id: i32,
) -> Result<Duration, grammers_mtsender::InvocationError> {
    let started_at = Instant::now();
    client
        .invoke(&tl::functions::Ping {
            ping_id: i64::from(message_id),
        })
        .await?;
    Ok(started_at.elapsed())
}

fn log_ping_failure(action: &Action, message_id: i32, error: &grammers_mtsender::InvocationError) {
    tracing::warn!(
        event = "telegram_ping_failed",
        command = action.name(),
        message_id,
        error_category = invocation_error_category(error),
        "Telegram ping failed"
    );
}

fn external_event_error_category(error: &ExternalError) -> &'static str {
    match error {
        ExternalError::Unavailable => "unavailable",
        ExternalError::ExecutionTimeout => "timeout",
        ExternalError::ProtocolDecode
        | ExternalError::LineTooLarge
        | ExternalError::WrongRequestId
        | ExternalError::WrongModuleId => "protocol",
        ExternalError::ResultTooLarge => "result_too_large",
        ExternalError::ModuleError => "module_error",
        _ => "other",
    }
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

#[derive(Debug, Default)]
struct ProcStats {
    system_uptime: Option<Duration>,
    memory_kib: Option<u64>,
}

async fn read_proc_stats() -> ProcStats {
    tokio::task::spawn_blocking(|| ProcStats {
        system_uptime: std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|uptime| parse_system_uptime(&uptime)),
        memory_kib: std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| parse_memory_kib(&status)),
    })
    .await
    .unwrap_or_default()
}

fn log_unavailable_proc_stats(proc_stats: &ProcStats) {
    if proc_stats.system_uptime.is_none() {
        tracing::debug!(
            event = "proc_stat_unavailable",
            stat = "system_uptime",
            "Proc stat unavailable"
        );
    }
    if proc_stats.memory_kib.is_none() {
        tracing::debug!(
            event = "proc_stat_unavailable",
            stat = "memory",
            "Proc stat unavailable"
        );
    }
}

fn parse_system_uptime(input: &str) -> Option<Duration> {
    let seconds = input.split_whitespace().next()?.parse::<f64>().ok()?;
    (seconds.is_finite() && seconds >= 0.0)
        .then_some(seconds)
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
}

fn parse_memory_kib(input: &str) -> Option<u64> {
    input.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("VmRSS:") {
            return None;
        }
        let value = fields.next()?;
        (fields.next() == Some("kB"))
            .then(|| value.parse().ok())
            .flatten()
    })
}

fn format_latency(latency: Duration) -> String {
    if latency < Duration::from_millis(1) {
        "<1 ms".to_owned()
    } else {
        format!("{} ms", latency.as_millis())
    }
}

fn format_duration(duration: Duration) -> String {
    let mut seconds = duration.as_secs();
    let days = seconds / 86_400;
    seconds %= 86_400;
    let hours = seconds / 3_600;
    seconds %= 3_600;
    let minutes = seconds / 60;
    seconds %= 60;

    if days > 0 {
        format!("{days}d {hours:02}h {minutes:02}m {seconds:02}s")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

const LAVIS_SOURCE_URL: &str = "https://tangled.org/zumuvik.tngl.sh/lavis";

fn format_info(locale: Locale, prefix: &str, installed_external_modules: usize) -> String {
    let built_in_modules = crate::modules::modules().len();
    match locale {
        Locale::English => format!(
            "ℹ️ Lavis — really your userbot\n\nVersion: {}\nMTProto: grammers\nModule API: v6\nPrefix: {prefix}\nBuilt-in modules: {built_in_modules}\nInstalled external modules: {installed_external_modules}\nSource: {LAVIS_SOURCE_URL}",
            env!("CARGO_PKG_VERSION")
        ),
        Locale::Russian => format!(
            "ℹ️ Lavis — really your userbot\n\nВерсия: {}\nMTProto: grammers\nAPI модулей: v6\nПрефикс: {prefix}\nВстроенные модули: {built_in_modules}\nУстановленные внешние модули: {installed_external_modules}\nИсходники: {LAVIS_SOURCE_URL}",
            env!("CARGO_PKG_VERSION")
        ),
    }
}

fn format_stats(
    locale: Locale,
    telegram: &str,
    lavis_uptime: Duration,
    proc_stats: &ProcStats,
    recognized_commands: u64,
) -> String {
    let system_uptime = proc_stats
        .system_uptime
        .map(format_duration)
        .unwrap_or_else(|| stats_text(locale, StatsText::Unavailable).to_owned());
    let memory = proc_stats
        .memory_kib
        .map(|memory_kib| format!("{:.1} MiB RSS", memory_kib as f64 / 1024.0))
        .unwrap_or_else(|| stats_text(locale, StatsText::Unavailable).to_owned());

    render_stats_text(
        locale,
        telegram,
        &format_duration(lavis_uptime),
        &system_uptime,
        &memory,
        recognized_commands,
        env!("CARGO_PKG_VERSION"),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ProcStats, SensitiveCommandDenial, SensitiveCommandPolicy, authorize_sensitive_message,
        bounded_list, external_event_error_category, fastfetch_response, format_duration,
        format_latency, format_stats, lm_usage, missing_descriptor_response, parse_memory_kib,
        parse_system_uptime, render_install_plan, setup_status_label, setup_status_response,
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
        upstream::{UpstreamError, UpstreamRev, UpstreamRevFuture},
    };
    use grammers_session::types::PeerId;
    use std::{
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
    async fn bounds_replaces_and_cleans_up_expected_self_edits() {
        let (mut runtime, directory) = runtime_with_alias().await;
        let peer = grammers_session::types::PeerId::user(1).unwrap();
        for message_id in 0..=super::MAX_EXPECTED_SELF_EDITS as i32 {
            runtime.register_expected_self_edit(peer, message_id, format!("response {message_id}"));
        }
        assert!(!runtime.consume_expected_self_edit(peer, 0, "response 0"));
        assert!(runtime.consume_expected_self_edit(
            peer,
            super::MAX_EXPECTED_SELF_EDITS as i32,
            &format!("response {}", super::MAX_EXPECTED_SELF_EDITS)
        ));

        runtime.register_expected_self_edit(peer, 42, "old response".to_owned());
        runtime.register_expected_self_edit(peer, 42, "new response".to_owned());
        assert!(runtime.consume_expected_self_edit(peer, 42, "old response"));
        assert!(runtime.consume_expected_self_edit(peer, 42, "new response"));

        runtime.register_expected_self_edit(peer, 43, "failed response".to_owned());
        runtime.remove_expected_self_edit(peer, 43, "failed response");
        assert!(!runtime.consume_expected_self_edit(peer, 43, "failed response"));
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
        let execution = runtime.execute_info().await;

        assert!(execution.response.text.contains(&format!(
            "Modules: {}/{}",
            built_in_modules + 1,
            built_in_modules + 2
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
            username: Some("@owner".to_owned()),
            display_name: None,
            id: PeerId::self_user(),
        });
        runtime.set_upstream(Box::new(FakeUpstream::new(
            "b1d18f8ef407d043506c983b0d68e96c282eb1c9",
        )));

        let execution = runtime.execute_info().await;

        assert!(execution.response.text.contains("Owner: @owner"));
        assert!(execution.response.text.contains("Current commit: "));
        assert!(execution.response.text.contains("Upstream main: b1d18f8"));
        assert!(execution.response.text.contains("Prefix: "));
        assert!(execution.response.text.contains("Modules: "));
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
        runtime.set_upstream(Box::new(FakeUpstream::new_err()));

        let execution = runtime.execute_info().await;

        assert!(execution.response.text.contains("Upstream main: unavailable"));
        assert!(execution.media.is_some());
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

        let execution = runtime.execute_info().await;

        assert!(execution.response.text.contains("Owner: unknown"));
        fs::remove_dir_all(directory).ok();
    }

    struct FakeUpstream {
        result: Result<String, UpstreamError>,
    }

    impl FakeUpstream {
        fn new(revision: &str) -> Self {
            Self {
                result: Ok(revision.to_owned()),
            }
        }

        fn new_err() -> Self {
            Self {
                result: Err(UpstreamError::NoMainRef),
            }
        }
    }

    impl UpstreamRev for FakeUpstream {
        fn main_rev<'a>(&'a self) -> UpstreamRevFuture<'a> {
            Box::pin(async move { self.result.clone() })
        }
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
        assert!(output.contains("Version: 0.1.0"));
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
