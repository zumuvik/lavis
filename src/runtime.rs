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
    bot_api::{BotApi, HttpBotApi},
    command::Command,
    commands::{
        Action, AliasRequest, ExternalInvocation, LmRequest, ModulesRequest, PrefixRequest,
        SetupRequest, dispatch,
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
        manager::{ExternalManagerHandle, ExternalRuntimeSnapshot},
        state::ExternalStateStore,
    },
    fastfetch::{self, FastfetchInputError, FastfetchProfileError, FastfetchResult},
    help::{render_modules_overview_with_external, render_with_external},
    response::Response,
    settings::{DEFAULT_PREFIX, SettingsStore},
    setup::{self, UsernameCandidate},
    setup_store::SetupStore,
    setup_telegram::{BotFatherProgress, CompanionSetup, GrammersTelegramSetup, ProvisionRequest},
};

pub struct RuntimeState {
    started_at: Instant,
    recognized_commands: u64,
    aliases: AliasStore,
    settings: SettingsStore,
    fastfetch_profile_path: PathBuf,
    external_manager: Option<ExternalManagerHandle>,
    external_snapshot: ExternalRuntimeSnapshot,
    expected_self_edits: VecDeque<ExpectedSelfEdit>,
    setup_notification_ids: VecDeque<(PeerId, i32)>,
    setup_edit_fallback_sources: VecDeque<(PeerId, i32)>,
    setup: Option<SetupCoordinator>,
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
const MODULE_MUTATION_DENIED: &str =
    "⚠️ Эта операция с модулями доступна только из нового собственного сообщения в Saved Messages.";
const REBOOT_DENIED: &str = "⚠️ Перезапуск доступен только из нового сообщения.";

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
            external_manager: None,
            external_snapshot: ExternalRuntimeSnapshot::new(),
            expected_self_edits: VecDeque::new(),
            setup_notification_ids: VecDeque::new(),
            setup_edit_fallback_sources: VecDeque::new(),
            setup: None,
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
    }

    /// Marks a resolved BotFather peer as setup-private. Resolution is kept in
    /// the update layer because it needs Telegram APIs; this state only guards
    /// routing once a peer is known.
    pub fn set_setup_botfather_peer(&mut self, peer: PeerId) {
        if let Some(setup) = &mut self.setup {
            setup.botfather_peer = Some(peer);
        }
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
        // Only the ephemeral conversation ends. Persisted bot data remains
        // available to the repair path.
        setup.phase = SetupPhase::Idle;
        Some(Response::plain(
            "⌛ Настройка остановлена по таймауту. Постоянные данные сохранены; повторите setup или setup repair.".to_owned(),
        ))
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
            let outcome = setup.handle_botfather_reply(client, text).await;
            return SetupInput::Consumed {
                response: outcome.response,
                provision: outcome.provision,
            };
        }
        if !authored_by_self || peer != setup.saved_messages_peer || !setup.is_active() {
            return SetupInput::Ignored;
        }
        SetupInput::Consumed {
            response: Some(setup.handle_input(client, text).await),
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

    /// Claims the one Saved Messages fallback allowed for a setup input source.
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

    /// Resolve a dotted external command `module-id.command-name`.
    /// Uses the pre-computed set for sync routing.
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
        let handle = match self.external_manager.clone() {
            Some(h) => h,
            None => return Response::plain("⚠️ Внешние модули не доступны.".to_owned()),
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
                    Some(desc) => {
                        Response::external_result(text, &desc.display_name, &desc.id, &desc.version)
                    }
                    None => Response::plain(format!(
                        "{text}\n\n⚠️ Модуль «{}» не найден в описаниях.",
                        invocation.module_id
                    )),
                }
            }
            Err(ExternalError::Unavailable) => Response::plain(format!(
                "⚠️ Модуль «{}» недоступен или завершился с ошибкой.",
                invocation.module_id
            )),
            Err(ExternalError::ExecutionTimeout) => Response::plain(format!(
                "⚠️ Модуль «{}» не ответил вовремя.",
                invocation.module_id
            )),
            Err(ExternalError::ProtocolDecode) => Response::plain(format!(
                "⚠️ Модуль «{}» прислал некорректный ответ.",
                invocation.module_id
            )),
            Err(ExternalError::WrongRequestId) => Response::plain(format!(
                "⚠️ Модуль «{}» прислал ответ с неверным идентификатором.",
                invocation.module_id
            )),
            Err(ExternalError::ModuleError) => Response::plain(format!(
                "⚠️ Модуль «{}» сообщил об ошибке выполнения.",
                invocation.module_id
            )),
            Err(ExternalError::ResultTooLarge) => Response::plain(format!(
                "⚠️ Результат модуля «{}» слишком большой.",
                invocation.module_id
            )),
            Err(error) => {
                tracing::warn!(
                    event = "external_command_error",
                    module_id = %invocation.module_id,
                    command = %invocation.command_name,
                    error = %error,
                    "External command failed"
                );
                Response::plain(format!(
                    "⚠️ Ошибка модуля «{}»: {}",
                    invocation.module_id, error
                ))
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
        if let Action::Setup(request) = action {
            return self.execute_setup(client, request, peer_id).await;
        }
        match action {
            Action::Ping => match telegram_ping(client, message_id).await {
                Ok(latency) => Response::plain(format!("🏓 Pong: {}", format_latency(latency))),
                Err(error) => {
                    log_ping_failure(action, message_id, &error);
                    Response::plain("⚠️ Telegram ping failed")
                }
            },
            Action::Stats => {
                let telegram = match telegram_ping(client, message_id).await {
                    Ok(latency) => format_latency(latency),
                    Err(error) => {
                        log_ping_failure(action, message_id, &error);
                        "unavailable".to_owned()
                    }
                };
                let proc_stats = read_proc_stats().await;
                log_unavailable_proc_stats(&proc_stats);
                Response::plain(format_stats(
                    &telegram,
                    self.started_at.elapsed(),
                    &proc_stats,
                    self.recognized_commands,
                ))
            }
            Action::Help(request) => {
                let rendered = render_with_external(
                    request,
                    &prefix,
                    &self.aliases,
                    self.external_command_refs(),
                    self.external_descriptors(),
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
                fastfetch::run(arguments, &self.fastfetch_profile_path).await,
                &prefix,
                &self.fastfetch_profile_path,
            ),
            Action::Alias(request) => self.execute_alias(request, &prefix).await,
            Action::Prefix(request) => self.execute_prefix(request).await,
            Action::Modules(request) => self.execute_modules(request, &prefix),
            Action::Lm(request) => self.execute_lm(client, message_context, request).await,
            Action::Reboot => return self.execute_reboot(message_context),
            Action::Setup(_) => unreachable!("setup actions return before response dispatch"),
            Action::External(invocation) => self.execute_external(invocation).await,
        }
        .into()
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
            LmRequest::Invalid => Response::plain(lm_usage(self.prefix())),
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
            return Response::plain("⚠️ Управление внешними модулями недоступно.".to_owned());
        };
        let state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain("⚠️ Состояние внешних модулей недоступно.".to_owned());
            }
        };
        let list = match control::list_modules(&config.root, &config.declarative_state_path, &state)
        {
            Ok(list) => list,
            Err(_) => {
                return Response::plain(
                    "⚠️ Не удалось прочитать список внешних модулей.".to_owned(),
                );
            }
        };
        let fresh_snapshot = match &self.external_manager {
            Some(handle) => handle.snapshot().await,
            None => self.external_snapshot.clone(),
        };
        let mut statuses = list.modules.iter().map(|entry| match &entry.module {
            Some(module) => format!("• {}\n  ID: {}\n  Версия: {}\n  Состояние: {}\n  Источник: {}\n  Runtime: {}\n  Автор: {}\n  Команд: {}", module.display_name, module.id, module.version, enabled_label(module.enabled), management_label(module.management), runtime_status_from_snapshot(&fresh_snapshot, &module.id), module.author, module.commands.len()),
            None => format!("• {}\n  Диагностика: {}\n  Состояние: {}\n  Источник: {}", entry.id.as_deref().unwrap_or("некорректный ID"), diagnostic_label(entry.diagnostic.as_ref()), enabled_label(entry.enabled), management_label(entry.management)),
        }).collect::<Vec<_>>();
        for id in state.enabled_ids() {
            if !list
                .modules
                .iter()
                .any(|entry| entry.id.as_deref() == Some(id))
            {
                statuses.push(format!("• {id}\n  Статус: включён, но каталог отсутствует"));
            }
        }
        if statuses.is_empty() {
            Response::plain(format!(
                "📦 Внешние модули не установлены.\n\nЧтобы установить модуль, прикрепите .lmod к сообщению:\n{}lm install",
                self.prefix()
            ))
        } else {
            Response::plain(format!("📦 Внешние модули\n\n{}", statuses.join("\n\n")))
        }
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
            return Response::plain("⚠️ Runtime внешних модулей недоступен.".to_owned());
        };
        match handle.diagnostic_text(id).await {
            Some(diagnostic) => Response::plain(format!(
                "📋 Последняя ошибка модуля {id}

{diagnostic}"
            )),
            None => Response::plain(format!(
                "ℹ️ Для модуля {id} нет сохранённой runtime-ошибки."
            )),
        }
    }

    /// Health checklist over the module runtime: per-module state, process
    /// status, and the last retained crash diagnostic (including failures that
    /// happened before the process left the running index). With an ID the
    /// report is narrowed to one module; without one it covers all modules.
    async fn lm_doctor(&self, id: Option<&str>) -> Response {
        let Some(config) = &self.module_control else {
            return Response::plain("⚠️ Управление внешними модулями недоступно.".to_owned());
        };
        let state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain("⚠️ Состояние внешних модулей недоступно.".to_owned());
            }
        };
        let list = match control::list_modules(&config.root, &config.declarative_state_path, &state)
        {
            Ok(list) => list,
            Err(_) => {
                return Response::plain(
                    "⚠️ Не удалось прочитать список внешних модулей.".to_owned(),
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
                    self.external_manager.as_ref(),
                    &self.external_snapshot,
                    &module.id,
                )
                .await;
                let diagnostic = if let Some(handle) = &self.external_manager {
                    handle
                        .diagnostic_summary(&module.id)
                        .await
                        .map(|summary| format!("\n  Последний сбой: {summary}"))
                } else {
                    None
                };
                lines.push(format!(
                    "• {}\n  ID: {}\n  Состояние: {}\n  Runtime: {}\n  Управление: {}{}",
                    module.display_name,
                    module.id,
                    enabled_label(module.enabled),
                    runtime,
                    management_label(module.management),
                    diagnostic.unwrap_or_default(),
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
            lines.push(format!(
                "• {enabled}\n  Статус: включён, но каталог отсутствует"
            ));
        }
        let Some(target) = id else {
            return Response::plain(format!(
                "🩺 Диагностика внешних модулей\n\n{}",
                if lines.is_empty() {
                    "Внешние модули не установлены.".to_owned()
                } else {
                    lines.join("\n\n")
                }
            ));
        };
        if lines.is_empty() {
            return Response::plain(format!("ℹ️ Модуль {target} не найден."));
        }
        Response::plain(format!(
            "🩺 Диагностика модуля {target}\n\n{}",
            lines.join("\n\n")
        ))
    }

    async fn lm_info(&self, id: &str) -> Response {
        let Some(config) = &self.module_control else {
            return Response::plain("⚠️ Управление внешними модулями недоступно.".to_owned());
        };
        let state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain("⚠️ Состояние внешних модулей недоступно.".to_owned());
            }
        };
        let diagnostic = if let Some(handle) = &self.external_manager {
            handle.diagnostic_text(id).await
        } else {
            None
        };
        match control::module_info(&config.root, &config.declarative_state_path, &state, id) {
            Ok(module) => Response::plain(format!(
                "📦 {}\nID: {}\nВерсия: {}\nАвтор: {}\nСостояние: {}\nИсточник: {}\nТочка входа: {}\nSchema/API protocol: v{}\nВозможности: {}\nПредоставляемые команды: {}\nRuntime: {}\nПоследняя ошибка: {}",
                module.display_name,
                module.id,
                module.version,
                module.author,
                enabled_label(module.enabled),
                management_label(module.management),
                module.entrypoint,
                module.protocol_version,
                capabilities_label(&module.capabilities),
                commands_label(&module.commands),
                fresh_runtime_status(
                    self.external_manager.as_ref(),
                    &self.external_snapshot,
                    &module.id
                )
                .await,
                diagnostic.as_deref().unwrap_or("нет")
            )),
            Err(control::ModuleControlError::InvalidInstalledModule) => {
                Response::plain("⚠️ Манифест установленного модуля некорректен.".to_owned())
            }
            Err(_) => Response::plain("⚠️ Модуль не установлен или недоступен.".to_owned()),
        }
    }

    async fn lm_set_enabled(&self, id: &str, enabled: bool) -> Response {
        let Some(config) = &self.module_control else {
            return Response::plain("⚠️ Управление внешними модулями недоступно.".to_owned());
        };
        let mut state = match ExternalStateStore::load(config.state_path.clone()).await {
            Ok(state) => state,
            Err(_) => {
                return Response::plain("⚠️ Состояние внешних модулей недоступно.".to_owned());
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
            Ok(operation) if operation.changed => Response::plain(format!(
                "✅ Модуль «{}» {}.\n\nДля применения изменений выполните:\n{}reboot",
                operation.module.display_name,
                if enabled { "включён" } else { "отключён" },
                self.prefix(),
            )),
            Ok(operation) => Response::plain(format!("ℹ️ Модуль «{}» уже {}.", operation.module.display_name, if enabled { "включён" } else { "отключён" })),
            Err(control::ModuleControlError::DeclarativelyManaged) => Response::plain("⚠️ Модуль управляется декларативно через NixOS. Измените services.lavis.extensions и выполните nh os switch.".to_owned()),
            Err(_) => Response::plain("⚠️ Не удалось изменить состояние модуля.".to_owned()),
        }
    }

    fn execute_reboot(&self, context: MessageExecutionContext<'_>) -> RuntimeExecution {
        match self.authorize_sensitive_command(SensitiveCommandPolicy::Reboot, context, None) {
            Ok(()) => RuntimeExecution {
                response: Response::plain("♻️ Lavis перезапускается…".to_owned()),
                provision: None,
                shutdown: None,
                post_edit: Some(PostEditAction::ArmRebootReceipt),
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
        .map_err(|reason| Response::plain(reason.response(policy).to_owned()))
    }

    async fn inspect_module_install(&mut self, client: &Client, message: &Message) -> Response {
        let Some(installation) = &self.module_installation else {
            return Response::plain("⚠️ Установка внешних модулей недоступна.".to_owned());
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
            Err(_) => return Response::plain(
                "⚠️ Прикрепите документ .lmod к новому собственному сообщению в Saved Messages."
                    .to_owned(),
            ),
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
                return Response::plain("⚠️ Пакет .lmod не прошёл безопасную проверку.".to_owned());
            }
        };
        let prefix = self.prefix().to_owned();
        match self.module_approvals.issue(pending) {
            Ok((id, _)) => match self.module_approvals.get(id) {
                Ok(plan) => Response::plain(render_install_plan(plan, id, &prefix)),
                Err(error) => {
                    tracing::warn!(
                        event = "external_module_approval_plan_unavailable",
                        error = %error,
                        "Issued external module approval could not be read back"
                    );
                    Response::plain("⚠️ Невозможно сохранить план установки.".to_owned())
                }
            },
            Err(_) => Response::plain("⚠️ Невозможно сохранить план установки.".to_owned()),
        }
    }

    async fn confirm_module_install(&mut self, supplied: &crate::commands::ApprovalId) -> Response {
        let Ok(id) = ApprovalId::parse(supplied.as_str()) else {
            return Response::plain("⚠️ ApprovalId недействителен или истёк.".to_owned());
        };
        let Some(installation) = &self.module_installation else {
            return Response::plain("⚠️ Установка внешних модулей недоступна.".to_owned());
        };
        let module_id = match self.module_approvals.get(id) {
            Ok(plan) => plan.module_id.clone(),
            Err(ApprovalError::Unavailable | ApprovalError::InvalidId) => {
                return Response::plain("⚠️ ApprovalId недействителен или истёк.".to_owned());
            }
            Err(_) => return Response::plain("⚠️ План установки недоступен.".to_owned()),
        };
        if let Some(handle) = &self.external_manager {
            let manager = handle.lock().await;
            if manager.descriptor_by_id(&module_id).is_some() {
                return Response::plain(format!(
                    "⚠️ Модуль «{module_id}» уже зарегистрирован; установка не начата."
                ));
            }
        }
        let pending = match self.module_approvals.redeem(id) {
            Ok(pending) => pending,
            Err(ApprovalError::Unavailable | ApprovalError::InvalidId) => {
                return Response::plain("⚠️ ApprovalId недействителен или истёк.".to_owned());
            }
            Err(_) => return Response::plain("⚠️ План установки недоступен.".to_owned()),
        };
        let Some(wrapper) = pending.stage.take_wrapper() else {
            return Response::plain("⚠️ Проверенный пакет недоступен.".to_owned());
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
                        Response::plain("⚠️ Установка не завершена: откат цели не удался; проверьте каталог модулей вручную.".to_owned())
                    }
                    crate::external_modules::installer::InstallError::PostInstallValidationFailed { .. } => {
                        Response::plain("⚠️ Установка не выполнена: финальная проверка не пройдена, цель удалена.".to_owned())
                    }
                    _ => Response::plain("⚠️ Установка не выполнена.".to_owned()),
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
                return Response::plain(format!(
                    "⚠️ Модуль «{module_id}» установлен, но не зарегистрирован из-за конфликтующего описания."
                ));
            }
        }
        self.refresh_snapshot().await;
        Response::plain(format!("✅ Модуль «{module_id}» установлен и выключен."))
    }

    fn cancel_module_install(&mut self, supplied: &crate::commands::ApprovalId) -> Response {
        match ApprovalId::parse(supplied.as_str()) {
            Ok(id) => match self.module_approvals.revoke(id) {
                Ok(true) => Response::plain("✅ План установки отменён.".to_owned()),
                Ok(false) => Response::plain("⚠️ ApprovalId недействителен или истёк.".to_owned()),
                Err(_) => Response::plain("⚠️ План установки недоступен.".to_owned()),
            },
            Err(_) => Response::plain("⚠️ ApprovalId недействителен или истёк.".to_owned()),
        }
    }

    async fn execute_setup(
        &mut self,
        client: &Client,
        request: &SetupRequest,
        peer: PeerId,
    ) -> RuntimeExecution {
        let Some(setup) = &mut self.setup else {
            return Response::plain("⚠️ Setup storage is unavailable.".to_owned()).into();
        };
        if peer != setup.saved_messages_peer {
            return Response::plain(
                "⚠️ Setup is available only in Saved Messages. Start it there with the active prefix."
                    .to_owned(),
            ).into();
        }
        setup.handle_command(client, request).await
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
                let rendered = render_modules_overview_with_external(
                    prefix,
                    self.external_descriptors(),
                    self.external_command_refs(),
                );
                if rendered.entity_fallback {
                    tracing::warn!(
                        event = "modules_entity_fallback",
                        "Module formatting was unavailable"
                    );
                }
                rendered.response
            }
            ModulesRequest::Invalid => {
                Response::plain(format!("⚠️ Использование: {prefix}modules"))
            }
        }
    }

    pub(crate) async fn execute_prefix(&mut self, request: &PrefixRequest) -> Response {
        match request {
            PrefixRequest::Show => Response::plain(format!("⚙️ Active prefix: {}", self.prefix())),
            PrefixRequest::Set(prefix) => match self.settings.set_prefix(prefix.clone()).await {
                Ok(()) => Response::plain(format!("⚙️ Command prefix set to: {}", self.prefix())),
                Err(error) => Response::plain(format!("⚠️ Could not change prefix: {error}")),
            },
            PrefixRequest::Reset => match self.settings.set_prefix(DEFAULT_PREFIX.to_owned()).await
            {
                Ok(()) => Response::plain(format!("⚙️ Command prefix reset to: {}", self.prefix())),
                Err(error) => Response::plain(format!("⚠️ Could not reset prefix: {error}")),
            },
            PrefixRequest::Invalid => Response::plain(format!(
                "⚠️ Usage: {}prefix [new-prefix|reset]",
                self.prefix()
            )),
        }
    }

    async fn execute_alias(&mut self, request: &AliasRequest, prefix: &str) -> Response {
        match request {
            AliasRequest::List => {
                let aliases = self.aliases.aliases();
                if aliases.is_empty() {
                    return Response::plain("🔗 No aliases configured");
                }
                Response::plain(format!(
                    "🔗 Aliases\n\n{}",
                    aliases
                        .iter()
                        .map(|(name, alias)| {
                            let args = if alias.args.is_empty() {
                                String::new()
                            } else {
                                format!(" {}", shell_words::join(&alias.args))
                            };
                            format!("{prefix}{name} → {prefix}{}{args}", alias.target)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                ))
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
                Ok(_) => Response::plain(format!("🔗 Added alias: {prefix}{name}")),
                Err(error) => Response::plain(format!("⚠️ Could not add alias: {error}")),
            },
            AliasRequest::Delete { name } => match self.aliases.delete(name).await {
                Ok(DeleteResult::Deleted) => {
                    Response::plain(format!("🔗 Deleted alias: {prefix}{name}"))
                }
                Ok(DeleteResult::NotFound) => {
                    Response::plain(format!("❓ Alias not found: {name}"))
                }
                Err(error) => Response::plain(format!("⚠️ Could not delete alias: {error}")),
            },
            AliasRequest::Show { name } => {
                let normalized_name = name.to_ascii_lowercase();
                let Some(alias) = self.aliases.lookup(name) else {
                    return Response::plain(format!(
                        "⚠️ Alias {prefix}{normalized_name} does not exist"
                    ));
                };
                let args = if alias.args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", shell_words::join(&alias.args))
                };
                Response::collapsed(
                    format!("🔗 {prefix}{normalized_name}"),
                    format!("Alias for:\n{prefix}{}{args}", alias.target),
                )
                .response
            }
            AliasRequest::Invalid => Response::plain(format!(
                "⚠️ Usage: {prefix}alias [list|add <name> <command> [arguments...]|show <name>|del <name>]"
            )),
        }
    }
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
    fn response(self, policy: SensitiveCommandPolicy) -> &'static str {
        match (policy, self) {
            (SensitiveCommandPolicy::ModuleMutation, _) => MODULE_MUTATION_DENIED,
            (SensitiveCommandPolicy::Reboot, _) => REBOOT_DENIED,
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

fn enabled_label(enabled: bool) -> &'static str {
    if enabled {
        "включён"
    } else {
        "отключён"
    }
}

fn management_label(management: control::ModuleManagement) -> &'static str {
    match management {
        control::ModuleManagement::Manual => "вручную",
        control::ModuleManagement::DeclarativeNixOs => "NixOS (декларативно)",
    }
}

fn diagnostic_label(diagnostic: Option<&control::ModuleDiagnostic>) -> &'static str {
    match diagnostic {
        Some(control::ModuleDiagnostic::InvalidModuleId) => "некорректный ID каталога",
        Some(control::ModuleDiagnostic::InvalidManifest) => "некорректный манифест",
        None => "нет",
    }
}

fn runtime_status_from_snapshot<'a>(snapshot: &'a ExternalRuntimeSnapshot, id: &str) -> &'a str {
    snapshot
        .module_statuses
        .iter()
        .find(|status| status.id == id)
        .map(|status| status.status)
        .unwrap_or("не запущен")
}

async fn fresh_runtime_status(
    handle: Option<&ExternalManagerHandle>,
    cached: &ExternalRuntimeSnapshot,
    id: &str,
) -> String {
    match handle {
        Some(handle) => runtime_status_from_snapshot(&handle.snapshot().await, id).to_owned(),
        None => runtime_status_from_snapshot(cached, id).to_owned(),
    }
}

fn capabilities_label(capabilities: &[ExternalCapability]) -> String {
    if capabilities.is_empty() {
        "нет".to_owned()
    } else {
        capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn commands_label(
    commands: &[crate::external_modules::manifest::ExternalCommandDescriptor],
) -> String {
    if commands.is_empty() {
        "нет".to_owned()
    } else {
        commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn lm_usage(prefix: &str) -> String {
    format!(
        "⚠️ Использование:\n{prefix}lm\n{prefix}lm list\n{prefix}lm info <id>\n{prefix}lm logs <id>\n{prefix}lm doctor [<id>]\n{prefix}lm install\n{prefix}lm confirm <ApprovalId>\n{prefix}lm cancel <ApprovalId>\n{prefix}lm enable <id>\n{prefix}lm disable <id>"
    )
}

fn bounded_list(values: &[String]) -> String {
    const MAX_ITEMS: usize = 8;
    const MAX_VALUE_CHARS: usize = 96;
    if values.is_empty() {
        return "нет".to_owned();
    }
    let mut rendered = values
        .iter()
        .take(MAX_ITEMS)
        .map(|value| value.chars().take(MAX_VALUE_CHARS).collect::<String>())
        .collect::<Vec<_>>();
    if values.len() > MAX_ITEMS {
        rendered.push(format!("ещё {}", values.len() - MAX_ITEMS));
    }
    rendered.join(", ")
}

fn render_install_plan(
    plan: &crate::external_modules::source_inspection::ModuleInstallPlan,
    approval_id: ApprovalId,
    prefix: &str,
) -> String {
    let source = match &plan.source_identity {
        crate::external_modules::source_inspection::SourceIdentity::Archive => {
            "архив .lmod".to_owned()
        }
        crate::external_modules::source_inspection::SourceIdentity::PinnedRepository(
            repository,
        ) => {
            format!(
                "репозиторий {} @ {}",
                repository.repository(),
                repository.revision()
            )
        }
    };
    format!(
        "📋 План установки\n\nИсточник: {source}\nМодуль: {} v{}\nПротокол: {}\nТочка входа: {}\nКоманда по умолчанию: {}\nSHA-256: {}\nОтпечаток: {}\nАрхив: {} Байт, файлов: {}, сжато: {} Байт, распаковано: {} Байт\nВозможности: {}\nПодписки: {}\nМетоды Telegram V6: {}\nДействия: {}\nПредупреждения: {}\n\nApprovalId: {approval_id}\nПодтвердите: {prefix}lm confirm {approval_id}\nОтменить: {prefix}lm cancel {approval_id}\nСрок действия: 10 минут.",
        plan.module_id,
        plan.module_version,
        plan.protocol_version,
        plan.entrypoint,
        plan.default_command.as_deref().unwrap_or("нет"),
        plan.archive_digest.as_hex(),
        plan.fingerprint,
        plan.archive.archive_bytes,
        plan.archive.file_count,
        plan.archive.compressed_bytes,
        plan.archive.expanded_bytes,
        bounded_list(&plan.capabilities),
        bounded_list(&plan.subscriptions),
        bounded_list(&plan.telegram_methods),
        bounded_list(&plan.actions),
        bounded_list(
            &plan
                .warnings
                .iter()
                .map(|warning| format!("{warning:?}"))
                .collect::<Vec<_>>()
        ),
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
    ) -> RuntimeExecution {
        if matches!(
            request,
            SetupRequest::Start | SetupRequest::Auto | SetupRequest::Username(_)
        ) {
            if self.is_active() {
                return Response::plain(
                    "⚠️ Настройка уже выполняется. Напишите cancel для отмены.".to_owned(),
                )
                .into();
            }
            if self.has_created_bot().await {
                return Response::plain(
                    "ℹ️ Бот уже создан. Используйте setup repair для восстановления workspace."
                        .to_owned(),
                )
                .into();
            }
        }
        if matches!(request, SetupRequest::Repair) {
            return self.repair(client).await;
        }
        match request {
            SetupRequest::Status => self.status().await,
            SetupRequest::Cancel => {
                if !self.is_active() {
                    return Response::plain("ℹ️ Нет активной настройки для отмены.".to_owned())
                        .into();
                }
                self.phase = SetupPhase::Idle;
                Response::plain("✅ Настройка отменена.".to_owned())
            }
            SetupRequest::Start => {
                self.phase = SetupPhase::AwaitingUsername {
                    automatic: false,
                    deadline: Instant::now() + SETUP_STAGE_TIMEOUT,
                };
                Response::plain("🤖 Введите желаемое имя бота, оканчивающееся на _bot.".to_owned())
            }
            SetupRequest::Auto => match setup::generate_candidate() {
                Ok(username) => self.confirm_or_start(username, true, 1).await,
                Err(_) => Response::plain("⚠️ Не удалось сгенерировать имя бота.".to_owned()),
            },
            SetupRequest::Username(value) => match setup::validate_username(value) {
                Ok(username) => self.confirm_or_start(username, false, 1).await,
                Err(_) => Response::plain(
                    "⚠️ Имя должно содержать 5–32 ASCII-букв, цифр или _ и оканчиваться на _bot."
                        .to_owned(),
                ),
            },
            SetupRequest::Repair => unreachable!("repair returns a provisioning request"),
            SetupRequest::Invalid => Response::plain(
                "⚠️ Использование: setup [auto|<username_bot>|status|repair|cancel]".to_owned(),
            ),
        }
        .into()
    }

    async fn confirm_or_start(
        &mut self,
        username: UsernameCandidate,
        automatic: bool,
        attempts: u8,
    ) -> Response {
        self.phase = SetupPhase::AwaitingConfirmation {
            username: username.clone(),
            automatic,
            attempts,
            deadline: Instant::now() + SETUP_STAGE_TIMEOUT,
        };
        Response::plain(format!(
            "📋 План настройки\n\n• Создать companion-бота @{} с именем «{}».\n• Создать или восстановить приватный Lavis workspace.\n• Присоединить ваш Telegram-аккаунт к официальному публичному сообществу @lavis_userbot.\n• Добавить workspace, бота и сообщество в папку Lavis.\n\nНапишите confirm для подтверждения или cancel для отмены.",
            username.display(),
            crate::setup_telegram::DISPLAY_NAME
        ))
    }

    async fn handle_input(&mut self, client: &Client, text: &str) -> Response {
        if matches!(
            setup::parse_confirmation(text),
            Some(setup::Confirmation::Cancelled)
        ) {
            self.phase = SetupPhase::Idle;
            return Response::plain("✅ Настройка отменена.".to_owned());
        }
        match &self.phase {
            SetupPhase::AwaitingUsername { .. } => self.handle_username_input(text).await,
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
                    self.start_flow(client, username.clone(), *automatic, *attempts)
                        .await
                } else {
                    Response::plain("⚠️ Напишите confirm или cancel.".to_owned())
                }
            }
            _ => Response::plain("ℹ️ Настройка ожидает ответ BotFather.".to_owned()),
        }
    }

    async fn handle_username_input(&mut self, text: &str) -> Response {
        let automatic = match &self.phase {
            SetupPhase::AwaitingUsername { automatic, .. } => *automatic,
            _ => return Response::plain("ℹ️ Настройка ожидает ответ BotFather.".to_owned()),
        };
        let generated = matches!(text.trim().to_ascii_lowercase().as_str(), "-" | "auto");
        let username = match generated {
            true => setup::generate_candidate()
                .map_err(|_| crate::setup::UsernameError::InvalidCharactersOrLength),
            false => setup::validate_username(text.trim()),
        };
        match username {
            Ok(username) => {
                self.confirm_or_start(username, automatic || generated, 1)
                    .await
            }
            Err(_) => {
                Response::plain("⚠️ Некорректное имя. Оно должно оканчиваться на _bot.".to_owned())
            }
        }
    }

    async fn start_flow(
        &mut self,
        client: &Client,
        username: UsernameCandidate,
        automatic: bool,
        attempts: u8,
    ) -> Response {
        let Ok((transport, peer)) = GrammersTelegramSetup::resolve(client).await else {
            return Response::plain("⚠️ Не удалось связаться с BotFather.".to_owned());
        };
        let mut flow =
            CompanionSetup::new(username, self.state_path.clone(), self.token_path.clone());
        if flow.start(&transport).await.is_err() {
            return Response::plain("⚠️ Не удалось начать диалог с BotFather.".to_owned());
        }
        self.botfather_peer = Some(peer);
        self.phase = SetupPhase::Running {
            flow,
            transport,
            automatic,
            attempts,
            deadline: Instant::now() + SETUP_STAGE_TIMEOUT,
        };
        Response::plain("⏳ Настройка начата. Ожидается ответ BotFather.".to_owned())
    }

    async fn handle_botfather_reply(&mut self, client: &Client, text: &str) -> BotFatherOutcome {
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
                    response: Some(Response::plain("⚠️ Проверка бота недоступна.".to_owned())),
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
                        let response = self.start_flow(client, username, true, next_attempt).await;
                        BotFatherOutcome {
                            response: Some(response),
                            provision: None,
                        }
                    }
                    Err(_) => BotFatherOutcome {
                        response: Some(Response::plain(
                            "⚠️ Не удалось сгенерировать новое имя бота.".to_owned(),
                        )),
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
                    response: Some(Response::plain(
                        "⚠️ BotFather отклонил имя. Введите другое имя, оканчивающееся на _bot."
                            .to_owned(),
                    )),
                    provision: None,
                }
            }
            Ok(BotFatherProgress::LimitReached) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(
                        "⚠️ BotFather сообщил о достигнутом лимите ботов.".to_owned(),
                    )),
                    provision: None,
                }
            }
            Ok(BotFatherProgress::FloodWait) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(
                        "⚠️ BotFather просит повторить попытку позже.".to_owned(),
                    )),
                    provision: None,
                }
            }
            Ok(BotFatherProgress::Unexpected) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(
                        "⚠️ Диалог с BotFather завершился из-за неожиданного ответа.".to_owned(),
                    )),
                    provision: None,
                }
            }
            Err(crate::setup_telegram::SetupTelegramError::Storage) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(
                        "⚠️ Данные бота не удалось безопасно сохранить. Настройка остановлена."
                            .to_owned(),
                    )),
                    provision: None,
                }
            }
            Err(crate::setup_telegram::SetupTelegramError::Timeout) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(
                        "⚠️ BotFather не ответил вовремя. Настройка остановлена.".to_owned(),
                    )),
                    provision: None,
                }
            }
            Err(_) => {
                self.phase = SetupPhase::Idle;
                BotFatherOutcome {
                    response: Some(Response::plain(
                        "⚠️ Проверка или сохранение бота завершились ошибкой. Настройка остановлена."
                            .to_owned(),
                    )),
                    provision: None,
                }
            }
        }
    }

    async fn status(&self) -> Response {
        let state_path = self.state_path.clone();
        let token_path = self.token_path.clone();
        match tokio::task::spawn_blocking(move || {
            SetupStore::new(state_path, token_path).load_state()
        })
        .await
        {
            Ok(Ok(state)) => Response::plain(format!(
                "⚙️ Setup status: {}\nBot: {}",
                state.status,
                state
                    .identities
                    .bot_username
                    .as_deref()
                    .unwrap_or("not configured")
            )),
            Ok(Err(crate::error::SetupStoreError::NotFound)) => {
                Response::plain("⚙️ Setup status: idle".to_owned())
            }
            Ok(Err(_)) | Err(_) => Response::plain(
                "⚠️ Setup status is unavailable because local state could not be read safely."
                    .to_owned(),
            ),
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

    async fn repair(&self, client: &Client) -> RuntimeExecution {
        let api = match HttpBotApi::new() {
            Ok(api) => api,
            Err(_) => {
                return Response::plain("⚠️ Проверка сохранённого бота недоступна.".to_owned())
                    .into();
            }
        };
        self.repair_with_api(client, &api).await
    }

    async fn repair_with_api(&self, client: &Client, bot_api: &impl BotApi) -> RuntimeExecution {
        match self.repair_preflight(bot_api).await {
            Ok(username) => RuntimeExecution {
                response: Response::plain(
                    "⏳ Восстановление companion workspace начато.".to_owned(),
                ),
                provision: Some(ProvisionRequest::new(
                    client.clone(),
                    self.state_path.clone(),
                    self.token_path.clone(),
                    username,
                )),
                shutdown: None,
                post_edit: None,
            },
            Err(response) => response.into(),
        }
    }

    async fn repair_preflight(&self, bot_api: &impl BotApi) -> Result<String, Response> {
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
                return Err(Response::plain(
                    "⚠️ Нет безопасных данных для восстановления.".to_owned(),
                ));
            }
            Ok(Err(_)) | Err(_) => {
                return Err(Response::plain(
                    "⚠️ Сохранённые данные нельзя безопасно проверить.".to_owned(),
                ));
            }
        };
        let username = match state.identities.bot_username.as_deref() {
            Some(username) if setup::validate_username(username).is_ok() => username.to_owned(),
            _ => {
                return Err(Response::plain(
                    "⚠️ Сохранённая идентификация бота неполна или небезопасна.".to_owned(),
                ));
            }
        };
        let persisted_bot_id = state.identities.bot_user_id;
        let identity = match tokio::time::timeout(Duration::from_secs(25), bot_api.get_me(&token))
            .await
        {
            Ok(Ok(identity)) => identity,
            Ok(Err(_)) | Err(_) => {
                return Err(Response::plain(
                    "⚠️ Сохранённый токен не удалось безопасно проверить. Восстановление не запущено."
                        .to_owned(),
                ));
            }
        };
        if !identity.username.eq_ignore_ascii_case(&username) {
            return Err(Response::plain(
                "⚠️ Сохранённый токен не соответствует сохранённому боту. Восстановление не запущено."
                    .to_owned(),
            ));
        }
        if let Some(bot_id) = persisted_bot_id {
            if identity.id != bot_id {
                return Err(Response::plain(
                    "⚠️ Сохранённый токен не соответствует сохранённому боту. Восстановление не запущено."
                        .to_owned(),
                ));
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
                return Err(Response::plain(
                    "⚠️ Проверенный идентификатор бота не удалось безопасно сохранить. Восстановление не запущено."
                        .to_owned(),
                ));
            }
        }
        Ok(username)
    }
}

fn fastfetch_response(
    result: FastfetchResult,
    prefix: &str,
    profile_path: &std::path::Path,
) -> Response {
    match result {
        FastfetchResult::Success(response) => response,
        FastfetchResult::Empty => fastfetch_failure("produced no output", prefix),
        FastfetchResult::TimedOut => fastfetch_failure("timed out", prefix),
        FastfetchResult::Unavailable => fastfetch_failure("is unavailable", prefix),
        FastfetchResult::NonZero { code, .. } => {
            fastfetch_failure(&format!("failed (exit code {code})"), prefix)
        }
        FastfetchResult::UnexpectedStatus => fastfetch_failure("ended unexpectedly", prefix),
        FastfetchResult::InvalidArguments(error) => fastfetch_failure(
            &format!("input error: {}", fastfetch_input_message(error)),
            prefix,
        ),
        FastfetchResult::ProfileError(error) => Response::plain(format!(
            "⚠️ Fastfetch profile error: {} at {profile_path:?}. See {prefix}help fastfetch",
            fastfetch_profile_error_message(error)
        )),
    }
}

fn fastfetch_profile_error_message(error: FastfetchProfileError) -> &'static str {
    match error {
        FastfetchProfileError::NotReadable => "NotReadable",
        FastfetchProfileError::Malformed => "Malformed",
        FastfetchProfileError::UnsupportedVersion => "UnsupportedVersion",
        FastfetchProfileError::TooLarge => "TooLarge",
        FastfetchProfileError::UnsafePath => "UnsafePath",
        FastfetchProfileError::InvalidLogo => "InvalidLogo",
        FastfetchProfileError::InvalidStructure => "InvalidStructure",
        FastfetchProfileError::InvalidSeparator => "InvalidSeparator",
        FastfetchProfileError::InvalidLogoPadding => "InvalidLogoPadding",
    }
}

fn fastfetch_failure(message: &str, prefix: &str) -> Response {
    Response::plain(format!(
        "⚠️ Fastfetch {message}. See {prefix}help fastfetch"
    ))
}

fn fastfetch_input_message(error: FastfetchInputError) -> &'static str {
    match error {
        FastfetchInputError::Tokenization => "invalid quoting",
        FastfetchInputError::UnsupportedOption => "unsupported option",
        FastfetchInputError::MissingValue => "option value is missing",
        FastfetchInputError::DuplicateOption => "option is repeated",
        FastfetchInputError::InvalidLogo => "invalid --logo value",
        FastfetchInputError::InvalidStructure => "invalid --structure value",
        FastfetchInputError::InvalidSeparator => "invalid --separator value",
        FastfetchInputError::InvalidLogoPadding => "invalid --logo-padding value",
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

fn format_stats(
    telegram: &str,
    lavis_uptime: Duration,
    proc_stats: &ProcStats,
    recognized_commands: u64,
) -> String {
    let system_uptime = proc_stats
        .system_uptime
        .map(format_duration)
        .unwrap_or_else(|| "unavailable".to_owned());
    let memory = proc_stats
        .memory_kib
        .map(|memory_kib| format!("{:.1} MiB RSS", memory_kib as f64 / 1024.0))
        .unwrap_or_else(|| "unavailable".to_owned());

    format!(
        "📊 Lavis stats\n\nTelegram: {telegram}\nLavis uptime: {}\nSystem uptime: {system_uptime}\nMemory: {memory}\nCommands: {recognized_commands}\nVersion: {}",
        format_duration(lavis_uptime),
        env!("CARGO_PKG_VERSION"),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        MODULE_MUTATION_DENIED, ProcStats, REBOOT_DENIED, SensitiveCommandDenial,
        SensitiveCommandPolicy, authorize_sensitive_message, bounded_list,
        external_event_error_category, fastfetch_response, format_duration, format_latency,
        format_stats, lm_usage, parse_memory_kib, parse_system_uptime, render_install_plan,
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
        setup_store::{CompanionToken, PersistedSetupState, SetupStore},
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
            warnings: vec![InspectionWarning::TelegramRawNotSandboxed],
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
        assert!(render_install_plan(&plan, approval_id, ".").contains("account.updateStatus"));
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
            SensitiveCommandDenial::Edited.response(SensitiveCommandPolicy::Reboot),
            REBOOT_DENIED
        );
        assert!(!REBOOT_DENIED.contains("модул"));

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
            SensitiveCommandDenial::NotSavedMessages.response(request),
            MODULE_MUTATION_DENIED
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

        assert!(response.text.contains("таймаут"));
        assert!(runtime.setup_timeout_deadline().is_none());
        assert!(runtime.setup_protects_message(botfather, false));
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

        // One valid installed module (disabled) with an executable entrypoint.
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
            "#!{}\nimport json, sys, time\nframe = json.loads(sys.stdin.readline())\nprint(json.dumps({{'protocol_version':6,'type':'initialized','request_id':frame['request_id'],'module_id':'sample'}}), flush=True)\nframe = json.loads(sys.stdin.readline())\nprint(json.dumps({{'protocol_version':6,'type':'health','request_id':frame['request_id']}}), flush=True)\ntime.sleep(0.4)\nsys.exit(7)\n",
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
        crate::external_modules::v6_process::set_test_state_base(directory.join("state-base"));
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
            runtime.external_snapshot.module_statuses[0].status, "активен",
            "the cached routing snapshot intentionally predates the crash"
        );
        for _ in 0..200 {
            if handle
                .snapshot()
                .await
                .module_statuses
                .iter()
                .any(|status| status.id == "sample" && status.status == "ошибка")
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
                .any(|status| status.id == "sample" && status.status == "ошибка")
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
            .confirm_or_start(crate::setup::generate_candidate().unwrap(), true, 1)
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

        let response = setup.handle_username_input("lavis_test_bot").await;

        assert_eq!(
            response.text,
            "📋 План настройки\n\n• Создать companion-бота @lavis_test_bot с именем «Lavis — really your userbot».\n• Создать или восстановить приватный Lavis workspace.\n• Присоединить ваш Telegram-аккаунт к официальному публичному сообществу @lavis_userbot.\n• Добавить workspace, бота и сообщество в папку Lavis.\n\nНапишите confirm для подтверждения или cancel для отмены."
        );
        assert!(matches!(
            setup.phase,
            super::SetupPhase::AwaitingConfirmation { .. }
        ));
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
        assert!(setup.repair_preflight(&wrong).await.is_err());

        let matching = RepairBotApi {
            identity: Ok(BotIdentity {
                id: 7,
                username: "LAVIS_TEST_BOT".to_owned(),
            }),
        };
        assert_eq!(
            setup.repair_preflight(&matching).await.unwrap(),
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
        assert!(setup.repair_preflight(&wrong).await.is_err());
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
        assert!(setup.repair_preflight(&matching).await.is_ok());
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

        assert_eq!(
            runtime
                .execute_alias(
                    &AliasRequest::Show {
                        name: "MISSING".to_owned()
                    },
                    "!"
                )
                .await,
            Response::plain("⚠️ Alias !missing does not exist")
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
        assert!(overview.text.starts_with("🧩 Модули Lavis: 3\n\n"));
        assert!(overview.text.contains("🦀fastfetch"));
        assert!(overview.text.contains("Команды (10)"));
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
        assert_eq!(
            String::from_utf16(&units[..offset]).unwrap(),
            "🧩 Модули Lavis: 3\n\n"
        );
        let body = String::from_utf16(&units[offset..offset + length]).unwrap();
        assert!(body.contains("Команды (10)"));
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
            "⚙️ Command prefix set to: ."
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
            "⚙️ Command prefix reset to: ,"
        );
        assert_eq!(runtime.prefix(), ",");
        assert!(
            runtime
                .execute_prefix(&crate::commands::PrefixRequest::Set("bad".to_owned()))
                .await
                .text
                .contains("Could not change")
        );
        assert_eq!(runtime.prefix(), ",");
        fs::remove_dir_all(directory).unwrap();
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
                "🦀",
                std::path::Path::new("/tmp/fastfetch.json"),
            ),
            Response::plain("⚠️ Fastfetch input error: invalid --logo value. See 🦀help fastfetch")
        );
        let profile_path = PathBuf::from("/tmp/profile\nfastfetch.json");
        let response = fastfetch_response(
            FastfetchResult::ProfileError(FastfetchProfileError::Malformed),
            "🦀",
            &profile_path,
        );
        assert!(response.text.contains("Malformed"));
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

    #[test]
    fn formats_stats_with_all_labels_and_values() {
        let output = format_stats(
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
    fn module_install_lists_are_bounded_and_mutation_denial_text_is_exact() {
        let values = (0..10)
            .map(|index| format!("value-{index}"))
            .collect::<Vec<_>>();
        assert_eq!(
            bounded_list(&values),
            "value-0, value-1, value-2, value-3, value-4, value-5, value-6, value-7, ещё 2"
        );
        assert_eq!(
            MODULE_MUTATION_DENIED,
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
            lm_usage("."),
            "⚠️ Использование:\n.lm\n.lm list\n.lm info <id>\n.lm logs <id>\n.lm doctor [<id>]\n.lm install\n.lm confirm <ApprovalId>\n.lm cancel <ApprovalId>\n.lm enable <id>\n.lm disable <id>"
        );
    }
}
