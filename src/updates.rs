use anyhow::Context;
use grammers_client::{
    client::UpdateStream,
    tl,
    update::{Message, Update},
};
use grammers_session::types::{PeerAuth, PeerId, PeerKind, PeerRef};
use std::{future::Future, time::Duration};
use tokio::task::JoinSet;

use crate::{
    command::parse,
    commands::{Action, dispatch},
    reboot_receipt::{
        ArmOutcome, PendingRebootReceipt, RebootReceiptCompletion, RebootReceiptCoordinatorError,
        RebootReceiptEditIntent, RebootReceiptEditor, RebootReceiptStore, ReceiptEditOutcome,
        ReceiptStoreError, ReceiptTarget, SystemClock, TokioSleeper,
        complete_pending_reboot_receipt,
    },
    runtime::{
        CreatedEventDispatchResult, MessageExecutionContext, PostEditAction, RuntimeState,
        ShutdownReason, invocation_error_category,
    },
    setup_telegram::{ProvisionOutcome, ProvisionRequest},
};

const MAX_EVENT_DISPATCH_TASKS: usize = 32;
const PROVISION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const REBOOT_RECEIPT_EDIT_TIMEOUT: Duration = Duration::from_secs(1);
const REBOOT_RECEIPT_COMPLETION_TIMEOUT: Duration = Duration::from_secs(4);
const UPDATE_STREAM_RETRY_BASE: Duration = Duration::from_millis(250);
const UPDATE_STREAM_RETRY_MAX: Duration = Duration::from_secs(5);
const UPDATE_STREAM_RESTART_AFTER: u32 = 3;

struct EventDispatches {
    tasks: JoinSet<()>,
}

enum UpdateOrEvent<U> {
    Update(U),
    Event(Option<Result<(), tokio::task::JoinError>>),
}

struct ProvisionTasks {
    tasks: JoinSet<ProvisionOutcome>,
}

impl ProvisionTasks {
    fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }
    fn try_spawn(&mut self, request: ProvisionRequest) -> bool {
        self.try_spawn_task(request.run())
    }

    fn try_spawn_task(
        &mut self,
        task: impl Future<Output = ProvisionOutcome> + Send + 'static,
    ) -> bool {
        if !self.tasks.is_empty() {
            return false;
        }
        self.tasks.spawn(task);
        true
    }
    async fn abort_and_drain(&mut self) {
        self.tasks.abort_all();
        let _ = tokio::time::timeout(PROVISION_SHUTDOWN_TIMEOUT, async {
            while let Some(result) = self.tasks.join_next().await {
                if let Err(error) = result { tracing::debug!(event = "provision_task_join_failed", error = %error, "Provision task stopped"); }
            }
        }).await;
    }
}

impl EventDispatches {
    fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }

    fn try_spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) -> bool {
        if self.tasks.len() >= MAX_EVENT_DISPATCH_TASKS {
            return false;
        }
        self.tasks.spawn(task);
        true
    }

    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn has_capacity(&self) -> bool {
        self.tasks.len() < MAX_EVENT_DISPATCH_TASKS
    }

    async fn next_update_or_event<U>(
        &mut self,
        update: impl Future<Output = U>,
    ) -> UpdateOrEvent<U> {
        if self.is_empty() {
            return UpdateOrEvent::Update(update.await);
        }
        tokio::select! {
            update = update => UpdateOrEvent::Update(update),
            completed = self.tasks.join_next() => UpdateOrEvent::Event(completed),
        }
    }

    async fn abort_and_drain(&mut self) {
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
    }
}

pub async fn run(
    stream: &mut UpdateStream,
    self_user_id: PeerId,
    client: &grammers_client::Client,
    runtime: &mut RuntimeState,
    receipt_store: &RebootReceiptStore,
) -> anyhow::Result<ShutdownReason> {
    consume_pending_reboot_receipt(client, runtime, self_user_id, receipt_store).await;
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut event_dispatches = EventDispatches::new();
    let mut provision_tasks = ProvisionTasks::new();
    let mut consecutive_update_errors = 0_u32;
    let mut update_retry_deadline = None;

    loop {
        let setup_timeout = runtime
            .setup_timeout_deadline()
            .map(|deadline| tokio::time::sleep_until(deadline.into()));
        let retry_deadline = update_retry_deadline;
        tokio::select! {
            signal = &mut shutdown => {
                signal.context("failed to listen for Ctrl-C shutdown signal")?;
                event_dispatches.abort_and_drain().await;
                provision_tasks.abort_and_drain().await;
                stream
                    .sync_update_state()
                    .await
                    .map_err(anyhow::Error::from_boxed)
                    .context("failed to synchronize Telegram update state")?;
                return Ok(ShutdownReason::Exit);
            }
            provision = provision_tasks.tasks.join_next(), if !provision_tasks.tasks.is_empty() => {
                match provision {
                    Some(Ok(outcome)) => send_provision_completion(client, runtime, outcome).await,
                    Some(Err(error)) => tracing::warn!(event = "provision_task_join_failed", error = %error, "Provision task failed"),
                    None => {}
                }
            }
            _ = async {
                if let Some(timeout) = setup_timeout {
                    timeout.await;
                }
            }, if setup_timeout.is_some() => {
                if let Some(response) = runtime.handle_setup_timeout() {
                    send_setup_notification(client, runtime, response).await;
                }
            }
            next = event_dispatches.next_update_or_event(async {
                if let Some(deadline) = retry_deadline {
                    tokio::time::sleep_until(deadline).await;
                }
                stream.next().await
            }) => {
                match next {
                    UpdateOrEvent::Event(Some(Ok(()))) => {}
                    UpdateOrEvent::Event(Some(Err(error))) => tracing::warn!(event = "external_event_task_failed", error = %error, "External event task failed"),
                    UpdateOrEvent::Event(None) => {}
                    UpdateOrEvent::Update(update) => {
                        let update = match update {
                            Ok(update) => {
                                if consecutive_update_errors > 0 {
                                    tracing::info!(
                                        event = "telegram_update_stream_recovered",
                                        consecutive_errors = consecutive_update_errors,
                                        "Telegram update stream recovered"
                                    );
                                }
                                consecutive_update_errors = 0;
                                update_retry_deadline = None;
                                update
                            }
                            Err(error) if is_temporary_telegram_error(&error) => {
                                consecutive_update_errors = consecutive_update_errors.saturating_add(1);
                                if consecutive_update_errors >= UPDATE_STREAM_RESTART_AFTER {
                                    tracing::warn!(
                                        event = "telegram_update_stream_restart",
                                        error_category = invocation_error_category(&error),
                                        error = %error,
                                        consecutive_errors = consecutive_update_errors,
                                        "Telegram update stream remains unavailable; restarting Lavis"
                                    );
                                    event_dispatches.abort_and_drain().await;
                                    provision_tasks.abort_and_drain().await;
                                    return Ok(ShutdownReason::Restart);
                                }
                                let retry_delay = update_stream_retry_delay(consecutive_update_errors);
                                update_retry_deadline = Some(tokio::time::Instant::now() + retry_delay);
                                tracing::warn!(
                                    event = "telegram_update_stream_retry",
                                    error_category = invocation_error_category(&error),
                                    error = %error,
                                    consecutive_errors = consecutive_update_errors,
                                    retry_in_ms = retry_delay.as_millis() as u64,
                                    "Telegram update stream temporarily failed; retrying"
                                );
                                continue;
                            }
                            Err(error) => {
                                event_dispatches.abort_and_drain().await;
                                provision_tasks.abort_and_drain().await;
                                return Err(anyhow::Error::new(error)
                                    .context("Telegram update stream ended or failed"));
                            }
                        };
                        // A BotFather RPC is part of processing this update. Keep it
                        // structured (rather than detached), but continue to honor
                        // shutdown and the owned setup deadline while it is pending.
                        let process_timeout = runtime.setup_timeout_deadline();
                        enum ProcessingResult {
                            Completed(Option<ShutdownReason>),
                            Shutdown(anyhow::Result<()>),
                            TimedOut,
                        }
                        let result = {
                            let processing = process_update(
                                update,
                                self_user_id,
                                client,
                                runtime,
                                receipt_store,
                                &mut event_dispatches,
                                &mut provision_tasks,
                            );
                            tokio::pin!(processing);
                            tokio::select! {
                                signal = &mut shutdown => ProcessingResult::Shutdown(signal.context("failed to listen for Ctrl-C shutdown signal")),
                                result = &mut processing => ProcessingResult::Completed(result),
                                _ = async {
                                    if let Some(deadline) = process_timeout {
                                        tokio::time::sleep_until(deadline.into()).await;
                                    }
                                }, if process_timeout.is_some() => ProcessingResult::TimedOut,
                            }
                        };
                        match result {
                            ProcessingResult::Completed(Some(reason)) => {
                                event_dispatches.abort_and_drain().await;
                                provision_tasks.abort_and_drain().await;
                                stream.sync_update_state().await.map_err(anyhow::Error::from_boxed).context("failed to synchronize Telegram update state")?;
                                return Ok(reason);
                            }
                            ProcessingResult::Completed(None) => {}
                            ProcessingResult::TimedOut => {
                                if let Some(response) = runtime.handle_setup_timeout() {
                                    send_setup_notification(client, runtime, response).await;
                                }
                            }
                            ProcessingResult::Shutdown(signal) => {
                                signal?;
                                event_dispatches.abort_and_drain().await;
                                provision_tasks.abort_and_drain().await;
                                stream
                                    .sync_update_state()
                                    .await
                                    .map_err(anyhow::Error::from_boxed)
                                    .context("failed to synchronize Telegram update state")?;
                                return Ok(ShutdownReason::Exit);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn update_stream_retry_delay(consecutive_errors: u32) -> Duration {
    let exponent = consecutive_errors.saturating_sub(1).min(5);
    UPDATE_STREAM_RETRY_BASE
        .saturating_mul(1_u32 << exponent)
        .min(UPDATE_STREAM_RETRY_MAX)
}

fn is_temporary_telegram_error(error: &grammers_client::InvocationError) -> bool {
    match error {
        grammers_client::InvocationError::Io(_)
        | grammers_client::InvocationError::Transport(_)
        | grammers_client::InvocationError::Dropped => true,
        grammers_client::InvocationError::Rpc(error) => {
            error.code == 420 && error.value.unwrap_or(u32::MAX) <= 5
                || matches!(error.code, 500 | 502 | 503)
        }
        _ => false,
    }
}

async fn send_setup_notification(
    client: &grammers_client::Client,
    runtime: &mut RuntimeState,
    response: crate::response::Response,
) {
    match client
        .send_message(
            &grammers_client::tl::types::InputPeerSelf {},
            grammers_client::message::InputMessage::new()
                .text(response.text)
                .fmt_entities(response.entities),
        )
        .await
    {
        Ok(message) => runtime.register_setup_notification(message.peer_id(), message.id()),
        Err(error) => tracing::warn!(
            event = "setup_notification_send_failed",
            error_category = invocation_error_category(&error),
            "Failed to send setup notification"
        ),
    }
}

async fn process_update(
    update: Update,
    self_user_id: PeerId,
    client: &grammers_client::Client,
    runtime: &mut RuntimeState,
    receipt_store: &RebootReceiptStore,
    event_dispatches: &mut EventDispatches,
    provision_tasks: &mut ProvisionTasks,
) -> Option<ShutdownReason> {
    let (message, edited) = match update {
        Update::NewMessage(message) => (message, false),
        Update::MessageEdited(message) => (message, true),
        _ => return None,
    };
    let message_id = message.id();
    let peer_id = message.peer_id();
    if runtime.consume_setup_notification(peer_id, message_id) {
        return None;
    }
    if edited && runtime.consume_expected_self_edit(peer_id, message_id, message.text()) {
        tracing::debug!(
            event = "command_self_edit_suppressed",
            message_id,
            "Suppressed the expected command response edit"
        );
        return None;
    }
    let outgoing = message.outgoing();
    let authored_by_self = is_self_authored(message.sender_id(), outgoing, self_user_id);
    tracing::debug!(
        event = "telegram_new_message",
        message_id,
        outgoing,
        authored_by_self,
        "Received Telegram message update"
    );

    // Setup is an exclusive interaction. Determine its routing before the
    // external message.created projection so neither setup replies nor a
    // resolved BotFather conversation can reach external modules.
    let action = route(authored_by_self, message.text(), runtime);
    let setup_input = if matches!(&action, Some(Action::Setup(_))) {
        None
    } else {
        match runtime
            .handle_setup_input(
                client,
                peer_id,
                authored_by_self,
                outgoing,
                edited,
                message.text(),
            )
            .await
        {
            crate::runtime::SetupInput::Ignored => None,
            crate::runtime::SetupInput::Consumed {
                response,
                provision,
            } => {
                if let Some(request) = provision
                    && !provision_tasks.try_spawn(request)
                {
                    tracing::warn!(
                        event = "provision_task_skipped",
                        "Provisioning already runs"
                    );
                }
                response
            }
        }
    };
    let event_protected = action.is_some()
        || setup_input.is_some()
        || runtime.setup_protects_message(peer_id, authored_by_self);

    // New command/setup messages stay private. If an already-projected message is
    // edited into protected content, emit a redacted edit so modules can reconcile
    // prior actions without receiving command or setup text.
    if should_prepare_message_event(edited, event_protected) {
        let event = if edited {
            crate::external_modules::protocol::MessageEventKind::Edited
        } else {
            crate::external_modules::protocol::MessageEventKind::Created
        };
        let event_text = if event_protected { "" } else { message.text() };
        let entities = if event_protected {
            Vec::new()
        } else {
            crate::external_modules::entities::project_custom_emoji_entities(
                message.fmt_entities(),
                0,
                message.text().encode_utf16().count(),
            )
        };
        if !event_dispatches.has_capacity() {
            tracing::warn!(
                event = "external_event_task_skipped",
                capacity = MAX_EVENT_DISPATCH_TASKS,
                "Skipped external event dispatch because the task queue is full"
            );
        } else if let Some(dispatch) = runtime.prepare_message_event_dispatch(
            peer_id, message_id, event, event_text, outgoing, entities,
        ) {
            let reaction_message = message.clone();
            let reaction_client = client.clone();
            let spawned = event_dispatches.try_spawn(async move {
                let result = dispatch.execute().await;
                handle_event_dispatch(reaction_client, reaction_message, result).await;
            });
            debug_assert!(
                spawned,
                "event dispatch capacity was checked before spawning"
            );
        }
    }

    if let Some(response) = setup_input.as_ref().filter(|_| authored_by_self) {
        let rendered_text = response.text.clone();
        let input = grammers_client::message::InputMessage::new()
            .text(rendered_text.clone())
            .fmt_entities(response.entities.clone());
        runtime.register_expected_self_edit(peer_id, message_id, rendered_text.clone());
        if let Err(error) = message.edit(input).await {
            runtime.remove_expected_self_edit(peer_id, message_id, &rendered_text);
            if error.is("MESSAGE_NOT_MODIFIED") {
                tracing::debug!(
                    event = "setup_input_edit_not_modified",
                    message_id,
                    "Setup input already has the requested response"
                );
            } else {
                tracing::warn!(
                    event = "setup_input_edit_failed",
                    message_id,
                    error_category = invocation_error_category(&error),
                    "Failed to edit setup input"
                );
                if runtime.claim_setup_edit_fallback(peer_id, message_id) {
                    send_setup_notification(client, runtime, response.clone()).await;
                }
            }
        }
        return None;
    }
    if setup_input.is_some() {
        // BotFather replies are setup-private but are not ours to edit.
        if let Some(response) = setup_input {
            send_setup_notification(client, runtime, response).await;
        }
        return None;
    }

    let mut action = action?;
    if let Action::External(invocation) = &mut action {
        invocation.argument_entities = command_argument_entities(
            message.text(),
            runtime.prefix(),
            &invocation.arguments,
            message.fmt_entities(),
        );
    }
    tracing::debug!(
        event = "command_matched",
        command = action.name(),
        message_id,
        "Matched authenticated command"
    );

    let execution = runtime
        .execute(
            client,
            &action,
            message_id,
            peer_id,
            MessageExecutionContext {
                message: &message,
                edited,
                authored_by_self,
            },
        )
        .await;
    if let Some(request) = execution.provision
        && !provision_tasks.try_spawn(request)
    {
        tracing::warn!(
            event = "provision_task_skipped",
            "Provisioning already runs"
        );
    }
    let post_edit = execution.post_edit;
    let mut shutdown_reason = execution.shutdown;
    let reboot_target = if matches!(post_edit, Some(PostEditAction::ArmRebootReceipt)) {
        match receipt_store.load(&SystemClock).await {
            Ok(crate::reboot_receipt::LoadOutcome::Pending(_)) => {
                let text = "⚠️ Уже ожидается подтверждение предыдущего перезапуска.".to_owned();
                fallback_reboot_edit(&message, runtime, peer_id, message_id, text).await;
                return None;
            }
            Err(_) => {
                let text = "⚠️ Не удалось проверить подтверждение перезапуска.".to_owned();
                fallback_reboot_edit(&message, runtime, peer_id, message_id, text).await;
                return None;
            }
            Ok(_) if peer_id == self_user_id => Some(ReceiptTarget::SelfUser),
            Ok(_) => match message.peer_ref().await {
                Ok(Some(peer)) => Some(receipt_target_from_peer_ref(peer)),
                _ => {
                    let text = "⚠️ Не удалось подготовить подтверждение перезапуска.".to_owned();
                    fallback_reboot_edit(&message, runtime, peer_id, message_id, text).await;
                    return None;
                }
            },
        }
    } else {
        None
    };
    let rendered_text = execution.response.text;
    let input = grammers_client::message::InputMessage::new()
        .text(rendered_text.clone())
        .fmt_entities(execution.response.entities);
    runtime.register_expected_self_edit(peer_id, message_id, rendered_text.clone());
    let source_edit_succeeded = match message.edit(input).await {
        Ok(()) => {
            tracing::debug!(
                event = "command_edit_succeeded",
                command = action.name(),
                message_id,
                "Edited outgoing command message"
            );
            true
        }
        Err(error) => {
            runtime.remove_expected_self_edit(peer_id, message_id, &rendered_text);
            tracing::warn!(
                event = "command_edit_failed",
                command = action.name(),
                message_id,
                error_category = invocation_error_category(&error),
                error = %error,
                "Failed to edit outgoing command message"
            );
            false
        }
    };
    if matches!(post_edit, Some(PostEditAction::ArmRebootReceipt)) {
        if !source_edit_succeeded {
            let failure = "⚠️ Не удалось начать перезапуск; Lavis продолжает работу.".to_owned();
            fallback_reboot_edit(&message, runtime, peer_id, message_id, failure).await;
            return None;
        }
        let target = reboot_target?;
        let started = match crate::reboot_receipt::Clock::unix_millis(&SystemClock) {
            Ok(value) => value,
            Err(_) => {
                fallback_reboot_edit(
                    &message,
                    runtime,
                    peer_id,
                    message_id,
                    "⚠️ Не удалось начать перезапуск; Lavis продолжает работу.".to_owned(),
                )
                .await;
                return None;
            }
        };
        let receipt = match PendingRebootReceipt::new(target, message_id, started) {
            Ok(receipt) => receipt,
            Err(_) => {
                fallback_reboot_edit(
                    &message,
                    runtime,
                    peer_id,
                    message_id,
                    "⚠️ Не удалось начать перезапуск; Lavis продолжает работу.".to_owned(),
                )
                .await;
                return None;
            }
        };
        match receipt_store.arm(receipt).await {
            Ok(ArmOutcome::Armed) | Err(ReceiptStoreError::ArmDurabilityUnknown { .. }) => {
                shutdown_reason = Some(ShutdownReason::Restart);
            }
            Ok(ArmOutcome::Conflict) | Err(_) => {
                let failure =
                    "⚠️ Не удалось сохранить подтверждение перезапуска; Lavis продолжает работу."
                        .to_owned();
                fallback_reboot_edit(&message, runtime, peer_id, message_id, failure).await;
                shutdown_reason = None;
            }
        }
    }
    shutdown_reason
}

pub(crate) fn receipt_target_from_peer_ref(peer: PeerRef) -> ReceiptTarget {
    if peer.id == PeerId::self_user() {
        return ReceiptTarget::SelfUser;
    }
    match peer.id.kind() {
        PeerKind::User => ReceiptTarget::User {
            id: peer.id.bare_id_unchecked(),
            access_hash: peer.auth.hash(),
        },
        PeerKind::Chat => ReceiptTarget::Chat {
            id: peer.id.bare_id_unchecked(),
        },
        PeerKind::Channel => ReceiptTarget::Channel {
            id: peer.id.bare_id_unchecked(),
            access_hash: peer.auth.hash(),
        },
    }
}

fn peer_ref_from_receipt_target(target: &ReceiptTarget) -> Option<PeerRef> {
    match *target {
        ReceiptTarget::SelfUser => Some(PeerId::self_user().to_ambient_ref()),
        ReceiptTarget::User { id, access_hash } => Some(PeerRef {
            id: PeerId::user(id)?,
            auth: PeerAuth::from_hash(access_hash),
        }),
        ReceiptTarget::Chat { id } => Some(PeerId::chat(id)?.to_ambient_ref()),
        ReceiptTarget::Channel { id, access_hash } => Some(PeerRef {
            id: PeerId::channel(id)?,
            auth: PeerAuth::from_hash(access_hash),
        }),
    }
}

async fn consume_pending_reboot_receipt(
    client: &grammers_client::Client,
    runtime: &mut RuntimeState,
    self_user_id: PeerId,
    receipt_store: &RebootReceiptStore,
) {
    let mut editor = TelegramRebootReceiptEditor {
        client,
        runtime,
        self_user_id,
    };
    let mut sleeper = TokioSleeper;
    let result = tokio::time::timeout(
        REBOOT_RECEIPT_COMPLETION_TIMEOUT,
        complete_pending_reboot_receipt(
            receipt_store,
            &SystemClock,
            &mut editor,
            &mut sleeper,
            Default::default(),
        ),
    )
    .await;
    match result {
        Ok(Ok(outcome)) => tracing::info!(
            event = "reboot_receipt_completion",
            outcome = reboot_completion_category(outcome),
            "Finished reboot receipt completion"
        ),
        Ok(Err(error)) => tracing::warn!(
            event = "reboot_receipt_completion_failed",
            category = reboot_coordinator_error_category(&error),
            "Could not complete reboot receipt"
        ),
        Err(_) => tracing::warn!(
            event = "reboot_receipt_completion_failed",
            category = "timeout",
            "Reboot receipt completion timed out"
        ),
    }
}

fn reboot_completion_category(outcome: RebootReceiptCompletion) -> &'static str {
    match outcome {
        RebootReceiptCompletion::Absent => "absent",
        RebootReceiptCompletion::Discarded => "discarded",
        RebootReceiptCompletion::Applied => "applied",
        RebootReceiptCompletion::AlreadyApplied => "already_applied",
        RebootReceiptCompletion::Terminal => "terminal",
        RebootReceiptCompletion::TemporaryExhausted => "temporary_exhausted",
    }
}

fn reboot_coordinator_error_category(error: &RebootReceiptCoordinatorError) -> &'static str {
    match error {
        RebootReceiptCoordinatorError::Store(_) => "store",
        RebootReceiptCoordinatorError::Clock(_) => "clock",
        RebootReceiptCoordinatorError::Validation(_) => "validation",
    }
}

struct TelegramRebootReceiptEditor<'a> {
    client: &'a grammers_client::Client,
    runtime: &'a mut RuntimeState,
    self_user_id: PeerId,
}

impl RebootReceiptEditor for TelegramRebootReceiptEditor<'_> {
    fn edit_reboot_receipt(
        &mut self,
        intent: RebootReceiptEditIntent,
    ) -> impl Future<Output = ReceiptEditOutcome> + Send {
        telegram_reboot_receipt_edit(self, intent)
    }
}

async fn telegram_reboot_receipt_edit(
    editor: &mut TelegramRebootReceiptEditor<'_>,
    intent: RebootReceiptEditIntent,
) -> ReceiptEditOutcome {
    let Some(peer) = peer_ref_from_receipt_target(intent.receipt.target()) else {
        return ReceiptEditOutcome::Terminal;
    };
    register_reboot_completion_suppression(editor.runtime, editor.self_user_id, peer.id, &intent);
    match tokio::time::timeout(
        REBOOT_RECEIPT_EDIT_TIMEOUT,
        editor.client.edit_message(
            peer,
            intent.receipt.message_id(),
            grammers_client::message::InputMessage::new().text(intent.text),
        ),
    )
    .await
    {
        Ok(Ok(())) => ReceiptEditOutcome::Applied,
        Ok(Err(error)) if error.is("MESSAGE_NOT_MODIFIED") => ReceiptEditOutcome::AlreadyApplied,
        Ok(Err(error)) if is_temporary_telegram_error(&error) => ReceiptEditOutcome::Temporary,
        Ok(Err(_)) => ReceiptEditOutcome::Terminal,
        Err(_) => ReceiptEditOutcome::Temporary,
    }
}

async fn fallback_reboot_edit(
    message: &Message,
    runtime: &mut RuntimeState,
    peer_id: PeerId,
    message_id: i32,
    text: String,
) {
    runtime.register_expected_self_edit(peer_id, message_id, text.clone());
    if message
        .edit(grammers_client::message::InputMessage::new().text(text.clone()))
        .await
        .is_err()
    {
        runtime.remove_expected_self_edit(peer_id, message_id, &text);
    }
}

fn register_reboot_completion_suppression(
    runtime: &mut RuntimeState,
    self_user_id: PeerId,
    peer_id: PeerId,
    intent: &RebootReceiptEditIntent,
) {
    let suppression_peer = match intent.receipt.target() {
        ReceiptTarget::SelfUser => self_user_id,
        _ => peer_id,
    };
    runtime.register_expected_self_edit(
        suppression_peer,
        intent.receipt.message_id(),
        intent.text.clone(),
    );
}

fn should_prepare_message_event(edited: bool, event_protected: bool) -> bool {
    edited || !event_protected
}

async fn handle_event_dispatch(
    client: grammers_client::Client,
    message: Message,
    result: CreatedEventDispatchResult,
) {
    for failure in result.failures {
        tracing::warn!(
            event = "external_event_failed",
            module_id = %failure.module_id,
            error_category = failure.category,
            "External event failed"
        );
    }
    for action in result.actions {
        let mut reactions = Vec::with_capacity(action.reactions.len());
        for reaction in action.reactions {
            match reaction {
                crate::external_modules::protocol::ReactionSpec::Emoji(emoticon) => {
                    reactions.push(tl::types::ReactionEmoji { emoticon }.into());
                }
                crate::external_modules::protocol::ReactionSpec::CustomEmoji { document_id } => {
                    let Ok(document_id) = document_id.parse::<i64>() else {
                        continue;
                    };
                    reactions.push(tl::types::ReactionCustomEmoji { document_id }.into());
                }
            }
        }
        let peer = match message.peer_ref().await {
            Ok(Some(peer)) => peer,
            Ok(None) => {
                tracing::warn!(
                    event = "external_reaction_peer_missing",
                    "External reaction peer reference is unavailable"
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(
                    event = "external_reaction_peer_failed",
                    error = %error,
                    "Could not resolve peer for an external reaction"
                );
                continue;
            }
        };
        if let Err(error) = client
            .invoke(&tl::functions::messages::SendReaction {
                big: false,
                add_to_recent: false,
                peer: peer.into(),
                msg_id: message.id(),
                reaction: Some(reactions),
            })
            .await
        {
            tracing::warn!(
                event = "external_reaction_failed",
                error_category = invocation_error_category(&error),
                "External reaction action failed"
            );
        }
    }
}

async fn send_provision_completion(
    client: &grammers_client::Client,
    runtime: &mut RuntimeState,
    outcome: ProvisionOutcome,
) {
    let text = provision_completion_text(outcome, runtime.prefix());
    match client
        .send_message(
            &grammers_client::tl::types::InputPeerSelf {},
            grammers_client::message::InputMessage::new().text(text),
        )
        .await
    {
        Ok(message) => runtime.register_setup_notification(message.peer_id(), message.id()),
        Err(error) => tracing::warn!(
            event = "provision_completion_send_failed",
            error_category = invocation_error_category(&error),
            "Failed to send provisioning completion"
        ),
    }
}

fn provision_completion_text(outcome: ProvisionOutcome, prefix: &str) -> String {
    match outcome {
        ProvisionOutcome::Completed => {
            "✅ Companion workspace и официальное сообщество @lavis_userbot настроены.".to_owned()
        }
        ProvisionOutcome::CompletedWithoutCommunity(_) => format!(
            "⚠️ Companion workspace готов, но присоединиться к @lavis_userbot не удалось. Повторите {prefix}setup repair."
        ),
        ProvisionOutcome::CompletedWithoutFolder(
            crate::setup_provision::CompletedWithoutFolder::Capacity,
        ) => format!(
            "⚠️ Companion workspace настроен без папки: достигнут лимит папок. Повторите {prefix}setup repair позже."
        ),
        ProvisionOutcome::CompletedWithoutFolder(
            crate::setup_provision::CompletedWithoutFolder::NameOrOwnershipConflict,
        ) => format!(
            "⚠️ Companion workspace настроен без папки: папка занята или принадлежит другой настройке. Повторите {prefix}setup repair после устранения конфликта."
        ),
        ProvisionOutcome::Failed(_) => format!(
            "⚠️ Восстановление companion workspace не завершено. Повторите {prefix}setup repair позже."
        ),
    }
}

fn command_argument_entities(
    text: &str,
    prefix: &str,
    arguments: &str,
    entities: Option<&Vec<grammers_client::tl::enums::MessageEntity>>,
) -> Vec<crate::external_modules::protocol::CustomEmojiEntity> {
    if arguments.is_empty() {
        return Vec::new();
    }
    let Some(command_text) = text.strip_prefix(prefix) else {
        return Vec::new();
    };
    let command_text = command_text.trim_start();
    let Some((_, trailing)) = command_text.split_once(char::is_whitespace) else {
        return Vec::new();
    };
    let argument_text = trailing.trim();
    if argument_text != arguments {
        return Vec::new();
    }
    let start_byte = argument_text.as_ptr() as usize - text.as_ptr() as usize;
    let start_utf16 = text[..start_byte].encode_utf16().count();
    crate::external_modules::entities::project_custom_emoji_entities(
        entities,
        start_utf16,
        start_utf16 + argument_text.encode_utf16().count(),
    )
}

fn is_self_authored(sender_id: Option<PeerId>, outgoing: bool, self_user_id: PeerId) -> bool {
    match sender_id {
        Some(sender_id) if sender_id == PeerId::self_user() => outgoing,
        Some(sender_id) => sender_id == self_user_id,
        None => false,
    }
}

fn route(authored_by_self: bool, text: &str, runtime: &RuntimeState) -> Option<Action> {
    let command = authored_by_self
        .then(|| parse(text, runtime.prefix()))
        .flatten()?;
    // Order: built-in > external namespaced > external default > alias.
    dispatch(&command)
        .or_else(|| runtime.resolve_external(&command.name, &command.args))
        .or_else(|| {
            if runtime.has_external_module(&command.name) {
                runtime.resolve_external_default(&command.name, &command.args)
            } else {
                runtime.resolve_alias(&command.name, &command.args)
            }
        })
}

#[cfg(test)]
mod tests {
    use grammers_session::types::PeerId;
    use tokio::sync::oneshot;

    use super::{
        EventDispatches, MAX_EVENT_DISPATCH_TASKS, ProvisionTasks, UPDATE_STREAM_RESTART_AFTER,
        UPDATE_STREAM_RETRY_BASE, UPDATE_STREAM_RETRY_MAX, UpdateOrEvent, is_self_authored,
        is_temporary_telegram_error, provision_completion_text,
        register_reboot_completion_suppression, route, should_prepare_message_event,
        update_stream_retry_delay,
    };
    use crate::commands::{Action, ExternalInvocation, PrefixRequest};
    use crate::{
        aliases::{Alias, AliasStore},
        external_modules::{
            manager::ExternalRuntimeSnapshot,
            manifest::{ExternalCommandDescriptor, ExternalModuleDescriptor},
        },
        reboot_receipt::{PendingRebootReceipt, ReceiptTarget, reboot_completion_text},
        runtime::RuntimeState,
        settings::SettingsStore,
    };
    use std::{
        collections::HashMap,
        path::PathBuf,
        time::{Duration, Instant},
    };

    #[test]
    fn update_stream_retry_delay_backs_off_and_caps() {
        assert_eq!(UPDATE_STREAM_RESTART_AFTER, 3);
        assert_eq!(update_stream_retry_delay(1), UPDATE_STREAM_RETRY_BASE);
        assert_eq!(update_stream_retry_delay(2), Duration::from_millis(500));
        assert_eq!(update_stream_retry_delay(3), Duration::from_secs(1));
        assert_eq!(update_stream_retry_delay(4), Duration::from_secs(2));
        assert_eq!(update_stream_retry_delay(5), Duration::from_secs(4));
        assert_eq!(update_stream_retry_delay(6), UPDATE_STREAM_RETRY_MAX);
        assert_eq!(update_stream_retry_delay(u32::MAX), UPDATE_STREAM_RETRY_MAX);
    }

    #[test]
    fn update_stream_retries_transient_transport_errors() {
        assert!(is_temporary_telegram_error(
            &grammers_client::InvocationError::Dropped
        ));
        assert!(is_temporary_telegram_error(
            &grammers_client::InvocationError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed",
            ))
        ));
    }

    #[tokio::test]
    async fn event_dispatches_limit_pending_tasks_and_skip_overload() {
        let mut dispatches = EventDispatches::new();
        assert_eq!(MAX_EVENT_DISPATCH_TASKS, 32);

        for _ in 0..MAX_EVENT_DISPATCH_TASKS {
            assert!(dispatches.try_spawn(std::future::pending()));
        }
        assert!(!dispatches.try_spawn(std::future::pending()));

        dispatches.abort_and_drain().await;
        assert!(dispatches.is_empty());
    }

    #[tokio::test]
    async fn ready_update_is_processed_while_an_event_dispatch_is_pending() {
        let mut dispatches = EventDispatches::new();
        assert!(dispatches.try_spawn(std::future::pending()));

        assert!(matches!(
            dispatches.next_update_or_event(async { "update" }).await,
            UpdateOrEvent::Update("update")
        ));

        dispatches.abort_and_drain().await;
    }

    #[tokio::test]
    async fn completed_event_is_reaped_while_another_dispatch_is_pending() {
        let mut dispatches = EventDispatches::new();
        assert!(dispatches.try_spawn(std::future::pending()));
        assert!(dispatches.try_spawn(async {}));

        assert!(matches!(
            dispatches
                .next_update_or_event(std::future::pending::<()>())
                .await,
            UpdateOrEvent::Event(Some(Ok(())))
        ));
        assert_eq!(dispatches.tasks.len(), 1);

        dispatches.abort_and_drain().await;
    }

    #[tokio::test]
    async fn shutdown_aborts_and_drains_event_dispatches() {
        struct DropSignal(Option<oneshot::Sender<()>>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let mut dispatches = EventDispatches::new();
        let (dropped, received_drop) = oneshot::channel();
        assert!(dispatches.try_spawn(async move {
            let _signal = DropSignal(Some(dropped));
            std::future::pending::<()>().await;
        }));
        tokio::task::yield_now().await;

        dispatches.abort_and_drain().await;
        assert!(dispatches.is_empty());
        assert_eq!(received_drop.await, Ok(()));
    }

    #[tokio::test]
    async fn provisioning_has_capacity_one() {
        let mut tasks = ProvisionTasks::new();
        assert!(tasks.try_spawn_task(std::future::pending()));
        assert!(!tasks.try_spawn_task(std::future::pending()));
        tasks.abort_and_drain().await;
        assert!(tasks.tasks.is_empty());
    }

    #[tokio::test]
    async fn updates_continue_while_provisioning_is_pending() {
        let mut tasks = ProvisionTasks::new();
        assert!(tasks.try_spawn_task(std::future::pending()));
        let received = tokio::select! {
            update = async { "update" } => update,
            _ = tasks.tasks.join_next() => "provision",
        };
        assert_eq!(received, "update");
        tasks.abort_and_drain().await;
    }

    #[test]
    fn reboot_completion_text_is_exact() {
        assert_eq!(
            reboot_completion_text(35_033),
            "✅ Lavis перезагрузился\n\nВремя перезагрузки: 35 с"
        );
    }

    #[tokio::test]
    async fn reboot_completion_attempt_registers_expected_suppression_before_edit() {
        let mut runtime = runtime().await;
        let self_user = PeerId::user(1).unwrap();
        let intent = crate::reboot_receipt::RebootReceiptEditIntent {
            receipt: PendingRebootReceipt::new(ReceiptTarget::SelfUser, 42, 1).unwrap(),
            text: reboot_completion_text(428),
        };

        register_reboot_completion_suppression(
            &mut runtime,
            self_user,
            PeerId::self_user(),
            &intent,
        );
        assert!(runtime.consume_expected_self_edit(self_user, 42, &intent.text));
    }

    #[test]
    fn provisioning_completion_uses_only_safe_status_text() {
        assert_eq!(
            provision_completion_text(crate::setup_telegram::ProvisionOutcome::Completed, "."),
            "✅ Companion workspace и официальное сообщество @lavis_userbot настроены."
        );
        assert_eq!(
            provision_completion_text(
                crate::setup_telegram::ProvisionOutcome::CompletedWithoutCommunity(
                    crate::setup_provision::ProvisionError::CommunityJoin,
                ),
                ".",
            ),
            "⚠️ Companion workspace готов, но присоединиться к @lavis_userbot не удалось. Повторите .setup repair."
        );
        assert_eq!(
            provision_completion_text(
                crate::setup_telegram::ProvisionOutcome::CompletedWithoutFolder(
                    crate::setup_provision::CompletedWithoutFolder::Capacity,
                ),
                ".",
            ),
            "⚠️ Companion workspace настроен без папки: достигнут лимит папок. Повторите .setup repair позже."
        );
        assert_eq!(
            provision_completion_text(
                crate::setup_telegram::ProvisionOutcome::CompletedWithoutFolder(
                    crate::setup_provision::CompletedWithoutFolder::NameOrOwnershipConflict,
                ),
                ".",
            ),
            "⚠️ Companion workspace настроен без папки: папка занята или принадлежит другой настройке. Повторите .setup repair после устранения конфликта."
        );
        assert_eq!(
            provision_completion_text(
                crate::setup_telegram::ProvisionOutcome::Failed(
                    crate::setup_grammers::ProvisionError::InviteBot,
                ),
                ".",
            ),
            "⚠️ Восстановление companion workspace не завершено. Повторите .setup repair позже."
        );
    }

    #[tokio::test]
    async fn shutdown_aborts_and_drains_provisioning() {
        struct DropSignal(Option<oneshot::Sender<()>>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let mut tasks = ProvisionTasks::new();
        let (dropped, received_drop) = oneshot::channel();
        assert!(tasks.try_spawn_task(async move {
            let _signal = DropSignal(Some(dropped));
            std::future::pending::<crate::setup_telegram::ProvisionOutcome>().await
        }));
        tokio::task::yield_now().await;
        tasks.abort_and_drain().await;
        assert!(tasks.tasks.is_empty());
        assert_eq!(received_drop.await, Ok(()));
    }

    #[tokio::test]
    async fn provisioning_notification_ids_are_not_routed_to_external_modules() {
        let mut runtime = runtime().await;
        let peer = PeerId::user(1).unwrap();
        runtime.register_setup_notification(peer, 42);
        assert!(runtime.consume_setup_notification(peer, 42));
        assert!(!runtime.consume_setup_notification(peer, 42));
    }

    async fn runtime() -> RuntimeState {
        RuntimeState::new(
            Instant::now(),
            AliasStore::load(PathBuf::from("/nonexistent/lavis-updates-aliases.json"))
                .await
                .unwrap(),
            SettingsStore::load(PathBuf::from("/nonexistent/lavis-updates-settings.json"))
                .await
                .unwrap(),
            PathBuf::from("/nonexistent/lavis-updates-fastfetch.json"),
        )
    }

    #[tokio::test]
    async fn routes_outgoing_false_messages_authored_by_self() {
        let outgoing = false;
        let authored_by_self = true;

        assert!(!outgoing);
        assert_eq!(
            route(authored_by_self, ",ping", &runtime().await),
            Some(Action::Ping)
        );
    }

    #[test]
    fn protected_command_messages_are_not_projected_to_external_events() {
        assert!(!should_prepare_message_event(false, true));
        assert!(should_prepare_message_event(true, true));
        assert!(should_prepare_message_event(false, false));
        assert!(should_prepare_message_event(true, false));
    }

    #[tokio::test]
    async fn rejects_outgoing_true_messages_not_authored_by_self() {
        let outgoing = true;
        let authored_by_self = false;

        assert!(outgoing);
        assert_eq!(route(authored_by_self, ",ping", &runtime().await), None);
        assert_eq!(route(authored_by_self, ",reboot", &runtime().await), None);
    }

    #[tokio::test]
    async fn ignores_self_authored_normal_unknown_and_dot_prefixed_text() {
        let runtime = runtime().await;
        assert_eq!(route(true, "ordinary outgoing text", &runtime), None);
        assert_eq!(route(true, ",unknown", &runtime), None);
        assert_eq!(route(true, ".ping", &runtime), None);
    }

    #[tokio::test]
    async fn routes_edited_style_text_with_the_active_prefix() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-updates-prefix-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let aliases = AliasStore::load(directory.join("aliases.json"))
            .await
            .unwrap();
        let mut settings = SettingsStore::load(directory.join("settings.json"))
            .await
            .unwrap();
        settings.set_prefix(".".to_owned()).await.unwrap();
        let runtime = RuntimeState::new(
            Instant::now(),
            aliases,
            settings,
            directory.join("fastfetch.json"),
        );
        assert_eq!(
            route(true, ".help", &runtime),
            Some(Action::Help(crate::commands::HelpRequest::Overview))
        );
        assert_eq!(route(true, ",help", &runtime), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn routes_modules_aliases_and_a_new_prefix_in_the_same_runtime() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-updates-routing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut aliases = AliasStore::load(directory.join("aliases.json"))
            .await
            .unwrap();
        aliases
            .add(
                "mods",
                Alias {
                    target: "modules".to_owned(),
                    args: Vec::new(),
                },
            )
            .await
            .unwrap();
        let settings = SettingsStore::load(directory.join("settings.json"))
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(
            Instant::now(),
            aliases,
            settings,
            directory.join("fastfetch.json"),
        );

        assert_eq!(
            route(true, ",modules", &runtime),
            Some(Action::Modules(crate::commands::ModulesRequest::Overview))
        );
        assert_eq!(
            route(true, ",mods", &runtime),
            Some(Action::Modules(crate::commands::ModulesRequest::Overview))
        );
        runtime
            .execute_prefix(&PrefixRequest::Set(".".to_owned()))
            .await;
        assert_eq!(
            route(true, ".modules", &runtime),
            Some(Action::Modules(crate::commands::ModulesRequest::Overview))
        );
        assert_eq!(route(true, ",modules", &runtime), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn routes_builtins_externals_defaults_and_aliases_in_priority_order() {
        fn descriptor(id: &str, default_command: Option<&str>) -> ExternalModuleDescriptor {
            ExternalModuleDescriptor {
                protocol_version: 3,
                id: id.to_owned(),
                display_name: id.to_owned(),
                version: "test".to_owned(),
                author: "test".to_owned(),
                entrypoint: PathBuf::new(),
                module_dir: PathBuf::new(),
                capabilities: vec![],
                default_command: default_command.map(str::to_owned),
                subscriptions: vec![],
                telegram_methods: vec![],
                actions: vec![],
                commands: vec![ExternalCommandDescriptor {
                    name: "run".to_owned(),
                    summary_ru: "test".to_owned(),
                    description_ru: "test".to_owned(),
                    usage: "".to_owned(),
                    examples: vec![],
                }],
            }
        }

        let directory = std::env::temp_dir().join(format!(
            "lavis-updates-routing-priority-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut aliases = AliasStore::load(directory.join("aliases.json"))
            .await
            .unwrap();
        for name in ["shortcut", "inactive", "crashed", "withoutdefault"] {
            aliases
                .add(
                    name,
                    Alias {
                        target: "ping".to_owned(),
                        args: vec![],
                    },
                )
                .await
                .unwrap();
        }
        let settings = SettingsStore::load(directory.join("settings.json"))
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(
            Instant::now(),
            aliases,
            settings,
            directory.join("fastfetch.json"),
        );
        runtime.set_external_snapshot_for_tests(ExternalRuntimeSnapshot {
            active_commands: ["external.run".to_owned()].into(),
            active_defaults: HashMap::from([
                ("default".to_owned(), "run".to_owned()),
                ("ping".to_owned(), "run".to_owned()),
            ]),
            descriptors: vec![
                descriptor("external", None),
                descriptor("default", Some("run")),
                descriptor("ping", Some("run")),
                descriptor("inactive", Some("run")),
                descriptor("crashed", Some("run")),
                descriptor("withoutdefault", None),
            ],
            ..ExternalRuntimeSnapshot::new()
        });

        assert_eq!(route(true, ",ping", &runtime), Some(Action::Ping));
        assert_eq!(
            route(true, ",external.run args", &runtime),
            Some(Action::External(ExternalInvocation {
                module_id: "external".to_owned(),
                command_name: "run".to_owned(),
                arguments: "args".to_owned(),
                argument_entities: vec![],
            }))
        );
        assert_eq!(
            route(true, ",default args", &runtime),
            Some(Action::External(ExternalInvocation {
                module_id: "default".to_owned(),
                command_name: "run".to_owned(),
                arguments: "args".to_owned(),
                argument_entities: vec![],
            }))
        );
        assert_eq!(route(true, ",shortcut", &runtime), Some(Action::Ping));
        assert_eq!(route(true, ",inactive", &runtime), None);
        assert_eq!(route(true, ",crashed", &runtime), None);
        assert_eq!(route(true, ",withoutdefault", &runtime), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn only_suppresses_the_exact_expected_edit_in_its_peer() {
        let mut runtime = runtime().await;
        let first_peer = PeerId::user(1).unwrap();
        let second_peer = PeerId::user(2).unwrap();
        runtime.register_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms".to_owned());

        assert!(!runtime.consume_expected_self_edit(first_peer, 7, ",ping"));
        assert_eq!(route(true, ",ping", &runtime), Some(Action::Ping));
        assert!(!runtime.consume_expected_self_edit(second_peer, 7, "🏓 Pong: 1 ms"));
        assert_eq!(route(true, ",ping", &runtime), Some(Action::Ping));
        assert!(runtime.consume_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms"));
        assert!(!runtime.consume_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms"));
    }

    #[test]
    fn accepts_concrete_self_sender_for_saved_messages() {
        let self_user_id = PeerId::user(1).unwrap();

        assert!(is_self_authored(Some(self_user_id), false, self_user_id));
    }

    #[test]
    fn accepts_self_sender_sentinel_only_for_outgoing_messages() {
        let self_user_id = PeerId::user(1).unwrap();

        assert!(is_self_authored(
            Some(PeerId::self_user()),
            true,
            self_user_id
        ));
    }

    #[test]
    fn rejects_other_user_sender() {
        let self_user_id = PeerId::user(1).unwrap();
        let other_user_id = PeerId::user(2).unwrap();

        assert!(!is_self_authored(Some(other_user_id), true, self_user_id));
    }

    #[test]
    fn rejects_outgoing_channel_sender() {
        let self_user_id = PeerId::user(1).unwrap();
        let channel_id = PeerId::channel(1).unwrap();

        assert!(!is_self_authored(Some(channel_id), true, self_user_id));
    }

    #[test]
    fn rejects_missing_sender() {
        let self_user_id = PeerId::user(1).unwrap();

        assert!(!is_self_authored(None, true, self_user_id));
    }
}
