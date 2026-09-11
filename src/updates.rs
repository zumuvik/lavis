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
    i18n::{RebootText, reboot_text},
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
const UPDATE_STREAM_RESTART_AFTER: u32 = 12;
const UPSTREAM_REFRESH_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);
const UPSTREAM_REFRESH_DEADLINE: Duration = Duration::from_secs(5);
const COLD_TRANSIENT_DELAYS: [Duration; 5] = [
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
];
const RATE_LIMIT_DELAYS: [Duration; 5] = [
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
    Duration::from_secs(900),
    Duration::from_secs(1800),
];

fn upstream_retry_delay(
    successful_snapshot: bool,
    transient_attempt: usize,
    rate_attempt: usize,
    rate_after: Option<Duration>,
    transient: bool,
    jitter: Duration,
    normal_cadence: Duration,
) -> Duration {
    let base = if rate_attempt > 0 {
        let index = (rate_attempt - 1).min(4);
        RATE_LIMIT_DELAYS[index]
    } else if !successful_snapshot && transient {
        let index = transient_attempt.min(4);
        COLD_TRANSIENT_DELAYS[index]
    } else {
        return normal_cadence;
    };
    let local = base.saturating_add(jitter);
    rate_after.map_or(local, |server| server.max(local))
}

const UPSTREAM_JITTER_MAX_MILLIS: u64 = 1000;

fn upstream_retry_jitter() -> Duration {
    let mut byte = [0_u8; 1];
    if getrandom::fill(&mut byte).is_err() {
        return Duration::ZERO;
    }
    Duration::from_millis((u64::from(byte[0]) * UPSTREAM_JITTER_MAX_MILLIS) / 256)
}

fn is_transient_refresh_failure(failure: &crate::runtime::UpstreamResolveFailure) -> bool {
    match failure {
        crate::runtime::UpstreamResolveFailure::MainRev(error) => matches!(
            error,
            crate::upstream::UpstreamError::Timeout
                | crate::upstream::UpstreamError::Transport
                | crate::upstream::UpstreamError::HttpStatus(500..=599)
        ),
        crate::runtime::UpstreamResolveFailure::RateLimited { .. } => false,
    }
}

enum UpstreamRefresh {
    Success(crate::upstream::UpstreamRevision),
    Version(Option<String>),
    Failed(String),
}

struct RefreshTask {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl RefreshTask {
    async fn cancel_and_join(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl Drop for RefreshTask {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

async fn upstream_refresh_task(
    resolver: Box<dyn crate::upstream::UpstreamRev>,
    sender: tokio::sync::mpsc::UnboundedSender<UpstreamRefresh>,
) {
    upstream_refresh_task_with(
        resolver,
        sender,
        UPSTREAM_REFRESH_INTERVAL,
        UPSTREAM_REFRESH_DEADLINE,
    )
    .await;
}

async fn upstream_refresh_task_with(
    resolver: Box<dyn crate::upstream::UpstreamRev>,
    sender: tokio::sync::mpsc::UnboundedSender<UpstreamRefresh>,
    interval_duration: Duration,
    deadline: Duration,
) {
    let mut successful_snapshot = false;
    let mut transient_attempt = 0usize;
    let mut rate_attempt = 0usize;
    let mut next_delay = Duration::ZERO;
    loop {
        if !next_delay.is_zero() {
            tokio::time::sleep(next_delay).await;
        }
        let started = tokio::time::Instant::now();
        let cycle_deadline = started + deadline;
        tracing::info!(
            event = "upstream_refresh_started",
            "Refreshing upstream snapshot"
        );
        let result = tokio::time::timeout_at(
            cycle_deadline,
            crate::runtime::resolve_upstream_revision(resolver.as_ref()),
        )
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(Ok(revision)) => {
                successful_snapshot = true;
                transient_attempt = 0;
                rate_attempt = 0;
                tracing::info!(event = "upstream_refresh_succeeded", elapsed_ms, main = %revision.revision, relation = ?revision.relation, "Upstream snapshot refreshed");
                if sender
                    .send(UpstreamRefresh::Success(revision.clone()))
                    .is_err()
                {
                    return;
                }
                match tokio::time::timeout_at(cycle_deadline, resolver.version()).await {
                    Ok(Ok(version)) => {
                        if sender.send(UpstreamRefresh::Version(version)).is_err() {
                            return;
                        }
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(
                            event = "upstream_version_refresh_failed",
                            category = %error,
                            "Could not refresh upstream package version; retaining the previous version"
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            event = "upstream_version_refresh_failed",
                            category = "timeout",
                            "Upstream package version refresh exceeded its deadline; retaining the previous version"
                        );
                    }
                }
            }
            Ok(Err(crate::runtime::UpstreamResolveFailure::MainRev(error))) => {
                tracing::warn!(event = "upstream_refresh_failed", elapsed_ms, category = %error, "Upstream refresh failed; retaining last successful snapshot");
                if sender
                    .send(UpstreamRefresh::Failed(error.to_string()))
                    .is_err()
                {
                    return;
                }
            }
            Ok(Err(crate::runtime::UpstreamResolveFailure::RateLimited { .. })) => {
                tracing::warn!(
                    event = "upstream_refresh_failed",
                    elapsed_ms,
                    category = "rate_limited",
                    "Upstream refresh failed; retaining last successful snapshot"
                );
                if sender
                    .send(UpstreamRefresh::Failed("rate_limited".to_owned()))
                    .is_err()
                {
                    return;
                }
            }
            Err(_) => {
                tracing::warn!(
                    event = "upstream_refresh_failed",
                    elapsed_ms,
                    category = "timeout",
                    "Upstream refresh exceeded its deadline; retaining last successful snapshot"
                );
                if sender
                    .send(UpstreamRefresh::Failed("timeout".to_owned()))
                    .is_err()
                {
                    return;
                }
            }
        }
        let rate_after = match &result {
            Ok(Err(crate::runtime::UpstreamResolveFailure::RateLimited { retry_after })) => {
                transient_attempt = 0;
                rate_attempt = rate_attempt.saturating_add(1);
                *retry_after
            }
            _ => None,
        };
        let transient = match &result {
            Ok(Err(failure)) => is_transient_refresh_failure(failure),
            Err(_) => true,
            _ => false,
        };
        let rate_limited = matches!(
            &result,
            Ok(Err(
                crate::runtime::UpstreamResolveFailure::RateLimited { .. }
            ))
        );
        if transient {
            rate_attempt = 0;
        } else if !matches!(
            &result,
            Ok(Err(
                crate::runtime::UpstreamResolveFailure::RateLimited { .. }
            ))
        ) && !matches!(&result, Ok(Ok(_)))
        {
            transient_attempt = 0;
            rate_attempt = 0;
        }
        next_delay = upstream_retry_delay(
            successful_snapshot,
            if transient {
                transient_attempt
            } else {
                COLD_TRANSIENT_DELAYS.len()
            },
            rate_attempt,
            rate_after,
            transient,
            if transient || rate_limited {
                upstream_retry_jitter()
            } else {
                Duration::ZERO
            },
            interval_duration,
        );
        if transient {
            transient_attempt = transient_attempt.saturating_add(1);
        }
        if matches!(&result, Ok(Ok(_))) {
            transient_attempt = 0;
            rate_attempt = 0;
        }
    }
}

async fn receive_refresh(
    receiver: &mut Option<&mut tokio::sync::mpsc::UnboundedReceiver<UpstreamRefresh>>,
) -> Option<UpstreamRefresh> {
    match receiver.as_mut() {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

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
    let resolver = runtime.take_upstream();
    let has_resolver = resolver.is_some();
    let (refresh_sender, mut refresh_receiver) = tokio::sync::mpsc::unbounded_channel();
    let refresh_task = resolver.map(|resolver| RefreshTask {
        handle: Some(tokio::spawn(upstream_refresh_task(
            resolver,
            refresh_sender,
        ))),
    });
    let result = run_loop(
        stream,
        self_user_id,
        client,
        runtime,
        receipt_store,
        has_resolver.then_some(&mut refresh_receiver),
    )
    .await;
    if let Some(task) = refresh_task {
        task.cancel_and_join().await;
    }
    result
}

async fn run_loop(
    stream: &mut UpdateStream,
    self_user_id: PeerId,
    client: &grammers_client::Client,
    runtime: &mut RuntimeState,
    receipt_store: &RebootReceiptStore,
    mut refresh_receiver: Option<&mut tokio::sync::mpsc::UnboundedReceiver<UpstreamRefresh>>,
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
            refresh = receive_refresh(&mut refresh_receiver) => {
                match refresh {
                    Some(UpstreamRefresh::Success(revision)) => {
                        runtime.publish_upstream_revision(Some(revision));
                    }
                    Some(UpstreamRefresh::Version(version)) => {
                        runtime.publish_upstream_version(version);
                    }
                    Some(UpstreamRefresh::Failed(category)) => {
                        runtime.publish_upstream_failure(category);
                    }
                    None => refresh_receiver = None,
                }
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

/// Classifies media-delivery failures for structured diagnostics. The photo
/// path has no single error type: uploads surface `std::io::Error` while the
/// send and delete steps surface `InvocationError`.
fn delivery_error_category(error: &anyhow::Error) -> &'static str {
    if error.source().is_some_and(|source| {
        source
            .downcast_ref::<grammers_client::InvocationError>()
            .is_some()
    }) {
        return "invocation";
    }
    "other"
}

/// `true` only when the error proves Telegram did not apply the edit, so the
/// expected-self-edit suppression may be released. An [`InvocationError::Rpc`]
/// arrives as a definitive server response: the mutation was rejected and never
/// applied, so the suppression cannot be matched by a later `MessageEdited`.
///
/// Every other [`InvocationError`] variant (`Io`, `Transport`, `Deserialize`,
/// `Dropped`, `Session`, `InvalidDc`, `Authentication`) is ambiguous: the
/// request may already have been fully sent and applied before the connection
/// broke, so the client cannot prove the edit did not happen. Those must fail
/// closed and keep the suppression armed.
fn edit_definitely_rejected(error: &grammers_client::InvocationError) -> bool {
    crate::message_provenance::edit_definitely_rejected(error)
}

/// Resolves the [`InvocationError`] (if any) hidden inside a media-delivery
/// `anyhow::Error`, so the fail-closed decision in
/// [`deliver_media_with_suppression`] is made on the underlying MTProto error
/// rather than the wrapping `anyhow` context.
fn as_invocation_error(error: &anyhow::Error) -> Option<&grammers_client::InvocationError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<grammers_client::InvocationError>())
}

/// Edits the outgoing command message in place to carry the static media URL
/// with `caption`. Telegram fetches the URL server-side, avoiding a local
/// upload on every invocation. The MTProto `messages.editMessage` call supports
/// the `media` parameter, so we replace the command text with the photo
/// rather than sending a new message and deleting the original.
async fn deliver_photo(
    command_message: &Message,
    media_url: &str,
    caption: String,
    entities: Vec<grammers_client::tl::enums::MessageEntity>,
) -> anyhow::Result<()> {
    let input = grammers_client::message::InputMessage::new()
        .text(caption)
        .fmt_entities(entities)
        .photo_url(media_url.to_owned());
    command_message
        .edit(input)
        .await
        .context("edit message with photo")?;
    Ok(())
}

/// The two edits a media-capable command response performs against the source
/// command message: replacing it with a photo, or falling back to a plain
/// text edit. Split out so the expected-self-edit bookkeeping around them is
/// unit-testable without a Telegram connection.
trait CommandMediaEdits {
    fn edit_photo(
        &mut self,
        media_url: &str,
        caption: &str,
        entities: Vec<grammers_client::tl::enums::MessageEntity>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    fn edit_text(
        &mut self,
        text: &str,
        entities: Vec<grammers_client::tl::enums::MessageEntity>,
    ) -> impl Future<Output = Result<(), grammers_client::InvocationError>> + Send;
}

struct TelegramCommandMediaEdits<'a> {
    message: &'a Message,
}

impl CommandMediaEdits for TelegramCommandMediaEdits<'_> {
    async fn edit_photo(
        &mut self,
        media_url: &str,
        caption: &str,
        entities: Vec<grammers_client::tl::enums::MessageEntity>,
    ) -> anyhow::Result<()> {
        deliver_photo(self.message, media_url, caption.to_owned(), entities).await
    }

    async fn edit_text(
        &mut self,
        text: &str,
        entities: Vec<grammers_client::tl::enums::MessageEntity>,
    ) -> Result<(), grammers_client::InvocationError> {
        self.message
            .edit(
                grammers_client::message::InputMessage::new()
                    .text(text.to_owned())
                    .fmt_entities(entities),
            )
            .await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaDeliveryOutcome {
    /// The photo edit succeeded. Its `MessageEdited` carries the media
    /// caption, which no longer parses as a command, so the matching
    /// suppression entry must stay armed.
    PhotoDelivered,
    /// The photo edit failed and the text fallback ran; `delivered` reports
    /// whether that fallback edit itself succeeded.
    TextFallbackApplied { delivered: bool },
}

/// Everything one media-capable response needs to edit the source command
/// message: where to suppress, what to deliver, and what to fall back to.
struct MediaDeliveryPlan<'a> {
    peer_id: PeerId,
    message_id: i32,
    command: &'a str,
    /// Caption for the photo edit; its suppression entry is armed before the
    /// round trip and removed when the photo edit fails.
    media_caption: &'a str,
    /// Text for the fallback edit; it arms its own suppression entry.
    fallback_text: &'a str,
    fallback_entities: Vec<grammers_client::tl::enums::MessageEntity>,
    media_url: &'a str,
}

/// Delivers a media response while keeping the expected-self-edit ledger
/// transactional:
///
/// 1. register the expected media caption BEFORE the Telegram round trip, so
///    the resulting `MessageEdited` can never be projected into external
///    modules as a user edit;
/// 2. when the photo edit fails, drop that registration first — no media
///    `MessageEdited` will ever arrive and the unfulfilled entry would linger
///    as stale suppression bound to this peer/message pair;
/// 3. arm the text fallback with its own expectation, and drop it again if
///    that edit fails too.
async fn deliver_media_with_suppression<E: CommandMediaEdits>(
    edits: &mut E,
    runtime: &mut RuntimeState,
    plan: MediaDeliveryPlan<'_>,
) -> MediaDeliveryOutcome {
    if runtime
        .register_expected_self_edit(plan.peer_id, plan.message_id, plan.media_caption.to_owned())
        .is_err()
    {
        return MediaDeliveryOutcome::TextFallbackApplied { delivered: false };
    }
    match edits
        .edit_photo(
            plan.media_url,
            plan.media_caption,
            plan.fallback_entities.clone(),
        )
        .await
    {
        Ok(()) => return MediaDeliveryOutcome::PhotoDelivered,
        Err(error) => {
            // Fail closed on ambiguous transport/read failures: the photo edit
            // may already have been applied, so its `MessageEdited` must not be
            // projected. Only a definitive server rejection proves it did not.
            let definitely_rejected =
                as_invocation_error(&error).is_some_and(edit_definitely_rejected);
            if definitely_rejected {
                runtime.remove_expected_self_edit(
                    plan.peer_id,
                    plan.message_id,
                    plan.media_caption,
                );
            }
            tracing::warn!(
                event = "command_media_delivery_failed",
                command = plan.command,
                message_id = plan.message_id,
                error_category = delivery_error_category(&error),
                fail_closed = !definitely_rejected,
                error = %error,
                "Falling back to a text edit"
            );
        }
    }
    if runtime
        .register_expected_self_edit(plan.peer_id, plan.message_id, plan.fallback_text.to_owned())
        .is_err()
    {
        return MediaDeliveryOutcome::TextFallbackApplied { delivered: false };
    }
    match edits
        .edit_text(plan.fallback_text, plan.fallback_entities)
        .await
    {
        Ok(()) => MediaDeliveryOutcome::TextFallbackApplied { delivered: true },
        Err(error) => {
            let definitely_rejected = edit_definitely_rejected(&error);
            if definitely_rejected {
                runtime.remove_expected_self_edit(
                    plan.peer_id,
                    plan.message_id,
                    plan.fallback_text,
                );
            }
            tracing::warn!(
                event = "command_edit_failed",
                command = plan.command,
                message_id = plan.message_id,
                error_category = invocation_error_category(&error),
                fail_closed = !definitely_rejected,
                error = %error,
                "Failed to edit outgoing command message after a media delivery failure"
            );
            MediaDeliveryOutcome::TextFallbackApplied { delivered: false }
        }
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
    // Via-bot inline menus are self-authored with module-controlled text:
    // consume them before routing, or a menu could execute as an owner
    // command (and pollute module event projections).
    if runtime.consume_bot_form_message(peer_id, message_id) {
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
    let authored_active_prefix = authored_by_self && message.text().starts_with(runtime.prefix());
    let event_protected = action.is_some()
        || setup_input.is_some()
        || runtime.setup_protects_message(peer_id, authored_by_self)
        || authored_active_prefix;

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
        if runtime
            .register_expected_self_edit(peer_id, message_id, rendered_text.clone())
            .is_err()
        {
            return None;
        }
        if let Err(error) = message.edit(input).await {
            let definitely_rejected = edit_definitely_rejected(&error);
            if definitely_rejected {
                runtime.remove_expected_self_edit(peer_id, message_id, &rendered_text);
            }
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
                    fail_closed = !definitely_rejected,
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
                replied: if let Action::External(invocation) = &action
                    && runtime.external_has_capability(
                        &invocation.module_id,
                        crate::external_modules::manifest::ExternalCapability::MessageRead,
                    ) {
                    tokio::time::timeout(Duration::from_millis(250), message.get_reply())
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .flatten()
                } else {
                    None
                },
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
    let locale = runtime.locale();
    let onboarding_page = execution.onboarding_page;
    let mut shutdown_reason = execution.shutdown;
    let reboot_target = if matches!(post_edit, Some(PostEditAction::ArmRebootReceipt)) {
        match receipt_store.load(&SystemClock).await {
            Ok(crate::reboot_receipt::LoadOutcome::Pending(_)) => {
                let text = reboot_text(locale, RebootText::Pending, None);
                fallback_reboot_edit(&message, runtime, peer_id, message_id, text).await;
                return None;
            }
            Err(_) => {
                let text = reboot_text(locale, RebootText::ReceiptLookupFailed, None);
                fallback_reboot_edit(&message, runtime, peer_id, message_id, text).await;
                return None;
            }
            Ok(_) if peer_id == self_user_id => Some(ReceiptTarget::SelfUser),
            Ok(_) => match message.peer_ref().await {
                Ok(Some(peer)) => Some(receipt_target_from_peer_ref(peer)),
                _ => {
                    let text = reboot_text(locale, RebootText::ReceiptPreparationFailed, None);
                    fallback_reboot_edit(&message, runtime, peer_id, message_id, text).await;
                    return None;
                }
            },
        }
    } else {
        None
    };
    let rendered_text = execution.response.text;
    let mut source_edit_succeeded = false;
    if let Some(media_url) = execution.media {
        // The media edit must be suppressed like any other Lavis-owned edit,
        // and its suppression entry is transactional: dropped when the photo
        // edit fails, re-armed by the text fallback, dropped again if the
        // fallback fails.
        let mut edits = TelegramCommandMediaEdits { message: &message };
        let plan = MediaDeliveryPlan {
            peer_id,
            message_id,
            command: action.name(),
            media_caption: &rendered_text,
            fallback_text: &rendered_text,
            fallback_entities: execution.response.entities,
            media_url: &media_url,
        };
        match deliver_media_with_suppression(&mut edits, runtime, plan).await {
            MediaDeliveryOutcome::PhotoDelivered => {
                tracing::info!(
                    event = "command_media_delivered",
                    command = action.name(),
                    message_id,
                    "Delivered reply as a photo message"
                );
                return shutdown_reason;
            }
            MediaDeliveryOutcome::TextFallbackApplied { delivered: true } => {
                tracing::debug!(
                    event = "command_edit_succeeded",
                    command = action.name(),
                    message_id,
                    "Edited outgoing command message after a failed media delivery"
                );
                source_edit_succeeded = true;
            }
            MediaDeliveryOutcome::TextFallbackApplied { delivered: false } => {}
        }
    } else {
        let input = grammers_client::message::InputMessage::new()
            .text(rendered_text.clone())
            .fmt_entities(execution.response.entities);
        source_edit_succeeded = if runtime
            .register_expected_self_edit(peer_id, message_id, rendered_text.clone())
            .is_err()
        {
            false
        } else {
            match message.edit(input).await {
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
                    // Fail closed on ambiguous transport/read failures: the edit
                    // may already have been applied, so its MessageEdited must stay
                    // suppressed. Only a definitive server rejection releases it.
                    if edit_definitely_rejected(&error) {
                        runtime.remove_expected_self_edit(peer_id, message_id, &rendered_text);
                    }
                    tracing::warn!(
                        event = "command_edit_failed",
                        command = action.name(),
                        message_id,
                        error_category = invocation_error_category(&error),
                        fail_closed = !edit_definitely_rejected(&error),
                        error = %error,
                        "Failed to edit outgoing command message"
                    );
                    false
                }
            }
        };
    }
    if source_edit_succeeded && onboarding_page {
        runtime.mark_onboarding_delivered().await;
    }
    if matches!(post_edit, Some(PostEditAction::ArmRebootReceipt)) {
        if !source_edit_succeeded {
            let failure = reboot_text(locale, RebootText::StartFailed, None);
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
                    reboot_text(locale, RebootText::StartFailed, None),
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
                    reboot_text(locale, RebootText::StartFailed, None),
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
                let failure = reboot_text(locale, RebootText::ReceiptArmFailed, None);
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
    let locale = runtime.locale();
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
            locale,
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
    if !register_reboot_completion_suppression(
        editor.runtime,
        editor.self_user_id,
        peer.id,
        &intent,
    ) {
        return ReceiptEditOutcome::Terminal;
    }
    match tokio::time::timeout(
        REBOOT_RECEIPT_EDIT_TIMEOUT,
        editor.client.edit_message(
            peer,
            intent.receipt.message_id(),
            grammers_client::message::InputMessage::new().text(intent.text.clone()),
        ),
    )
    .await
    {
        Ok(Ok(())) => ReceiptEditOutcome::Applied,
        Ok(Err(error)) if edit_definitely_rejected(&error) => {
            let suppression_peer = match intent.receipt.target() {
                ReceiptTarget::SelfUser => editor.self_user_id,
                _ => peer.id,
            };
            editor.runtime.remove_expected_self_edit(
                suppression_peer,
                intent.receipt.message_id(),
                &intent.text,
            );
            if error.is("MESSAGE_NOT_MODIFIED") {
                ReceiptEditOutcome::AlreadyApplied
            } else {
                ReceiptEditOutcome::Terminal
            }
        }
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
    if runtime
        .register_expected_self_edit(peer_id, message_id, text.clone())
        .is_err()
    {
        return;
    }
    if message
        .edit(grammers_client::message::InputMessage::new().text(text.clone()))
        .await
        .is_err_and(|error| edit_definitely_rejected(&error))
    {
        runtime.remove_expected_self_edit(peer_id, message_id, &text);
    }
}

fn register_reboot_completion_suppression(
    runtime: &mut RuntimeState,
    self_user_id: PeerId,
    peer_id: PeerId,
    intent: &RebootReceiptEditIntent,
) -> bool {
    let suppression_peer = match intent.receipt.target() {
        ReceiptTarget::SelfUser => self_user_id,
        _ => peer_id,
    };
    runtime
        .register_expected_self_edit(
            suppression_peer,
            intent.receipt.message_id(),
            intent.text.clone(),
        )
        .is_ok()
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
    let text = provision_completion_text(outcome, runtime.prefix(), runtime.locale());
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

fn provision_completion_text(
    outcome: ProvisionOutcome,
    prefix: &str,
    locale: crate::i18n::Locale,
) -> String {
    use crate::i18n::{SetupText, setup_text};
    let key = match outcome {
        ProvisionOutcome::Completed => SetupText::ProvisionCompleted,
        ProvisionOutcome::CompletedWithoutCommunity(_) => SetupText::ProvisionWithoutCommunity,
        ProvisionOutcome::CompletedWithoutFolder(
            crate::setup_provision::CompletedWithoutFolder::Capacity,
        ) => SetupText::ProvisionWithoutFolderCapacity,
        ProvisionOutcome::CompletedWithoutFolder(
            crate::setup_provision::CompletedWithoutFolder::NameOrOwnershipConflict,
        ) => SetupText::ProvisionWithoutFolderConflict,
        ProvisionOutcome::Failed(_) => SetupText::ProvisionFailed,
    };
    setup_text(locale, key).replace("{prefix}", prefix)
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
        CommandMediaEdits, EventDispatches, MAX_EVENT_DISPATCH_TASKS, MediaDeliveryOutcome,
        MediaDeliveryPlan, ProvisionTasks, RefreshTask, UPDATE_STREAM_RESTART_AFTER,
        UPDATE_STREAM_RETRY_BASE, UPDATE_STREAM_RETRY_MAX, UpdateOrEvent, UpstreamRefresh,
        deliver_media_with_suppression, edit_definitely_rejected, is_self_authored,
        is_temporary_telegram_error, provision_completion_text, receive_refresh,
        register_reboot_completion_suppression, route, should_prepare_message_event,
        update_stream_retry_delay, upstream_refresh_task_with, upstream_retry_delay,
    };
    use crate::commands::{Action, ExternalInvocation, PrefixRequest};
    use crate::upstream::{CompareFuture, UpstreamRev, UpstreamRevFuture};
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
        future::Future,
        path::PathBuf,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    #[test]
    fn update_stream_retry_delay_backs_off_and_caps() {
        assert_eq!(UPDATE_STREAM_RESTART_AFTER, 12);
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

    #[test]
    fn upstream_retry_schedule_is_capped_and_resets() {
        let cadence = Duration::from_secs(7200);
        assert_eq!(
            upstream_retry_delay(false, 0, 0, None, true, Duration::ZERO, cadence),
            Duration::from_secs(15)
        );
        assert_eq!(
            upstream_retry_delay(false, 1, 0, None, true, Duration::from_millis(137), cadence),
            Duration::from_secs(30) + Duration::from_millis(137)
        );
        assert_eq!(
            upstream_retry_delay(
                false,
                99,
                0,
                None,
                true,
                Duration::from_millis(548),
                cadence
            ),
            Duration::from_secs(300) + Duration::from_millis(4 * 137)
        );
        assert_eq!(
            upstream_retry_delay(true, 4, 0, None, true, Duration::from_secs(99), cadence),
            cadence
        );
    }

    #[test]
    fn explicit_retry_after_is_never_shortened() {
        let minimum = Duration::from_secs(17);
        assert_eq!(
            upstream_retry_delay(
                false,
                0,
                1,
                Some(minimum),
                true,
                Duration::from_millis(79),
                Duration::ZERO
            ),
            Duration::from_secs(60) + Duration::from_millis(79)
        );
        assert_eq!(
            upstream_retry_delay(
                true,
                0,
                5,
                Some(Duration::from_secs(600)),
                true,
                Duration::from_millis(395),
                Duration::ZERO
            ),
            Duration::from_secs(1800) + Duration::from_millis(395)
        );
        assert_eq!(
            upstream_retry_delay(
                true,
                0,
                1,
                None,
                true,
                Duration::from_millis(79),
                Duration::from_secs(7200)
            ),
            Duration::from_secs(60) + Duration::from_millis(79)
        );
    }

    #[test]
    fn refresh_backoff_sequences_are_deterministic_without_tight_loops() {
        let cadence = Duration::from_secs(2 * 60 * 60);
        assert_ne!(
            upstream_retry_delay(false, 0, 0, None, true, Duration::ZERO, cadence),
            upstream_retry_delay(false, 0, 0, None, true, Duration::from_millis(250), cadence)
        );
        let transient = [0, 1, 2, 3, 4].map(|attempt| {
            upstream_retry_delay(false, attempt, 0, None, true, Duration::ZERO, cadence)
        });
        assert_eq!(transient[0], Duration::from_secs(15));
        assert!(transient.windows(2).all(|pair| pair[1] > pair[0]));
        assert_eq!(transient[4], Duration::from_secs(300));

        // A successful snapshot returns to the normal cadence and clears either
        // kind of accumulated retry state.
        assert_eq!(
            upstream_retry_delay(true, 0, 0, None, false, Duration::from_secs(99), cadence),
            cadence
        );
        assert_eq!(
            upstream_retry_delay(true, 0, 0, None, true, Duration::from_secs(99), cadence),
            cadence
        );
        assert_eq!(
            upstream_retry_delay(true, 0, 0, None, false, Duration::from_secs(99), cadence),
            cadence
        );

        let repeated_rate_limits = (1..=5)
            .map(|attempt| {
                upstream_retry_delay(true, 0, attempt, None, false, Duration::ZERO, cadence)
            })
            .collect::<Vec<_>>();
        assert!(
            repeated_rate_limits
                .iter()
                .all(|delay| *delay >= Duration::from_secs(60))
        );
        assert!(
            repeated_rate_limits
                .windows(2)
                .all(|pair| pair[1] > pair[0])
        );
        assert_eq!(
            upstream_retry_delay(true, 0, 5, None, false, Duration::from_millis(395), cadence),
            Duration::from_secs(1800) + Duration::from_millis(395)
        );
        assert_eq!(
            upstream_retry_delay(
                true,
                0,
                1,
                Some(Duration::from_secs(10)),
                false,
                Duration::from_millis(79),
                cadence
            ),
            Duration::from_secs(60) + Duration::from_millis(79)
        );
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
            reboot_completion_text(crate::i18n::Locale::Russian, 35_033),
            "✅ Lavis перезагрузился\n\nВремя перезагрузки: 35 с"
        );
    }

    #[tokio::test]
    async fn reboot_completion_attempt_registers_expected_suppression_before_edit() {
        let mut runtime = runtime().await;
        let self_user = PeerId::user(1).unwrap();
        let intent = crate::reboot_receipt::RebootReceiptEditIntent {
            receipt: PendingRebootReceipt::new(ReceiptTarget::SelfUser, 42, 1).unwrap(),
            text: reboot_completion_text(crate::i18n::Locale::Russian, 428),
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
            provision_completion_text(
                crate::setup_telegram::ProvisionOutcome::Completed,
                ".",
                crate::i18n::Locale::English,
            ),
            "✅ Companion workspace and official community @lavis_userbot are configured."
        );
        assert_eq!(
            provision_completion_text(
                crate::setup_telegram::ProvisionOutcome::Completed,
                ".",
                crate::i18n::Locale::Russian
            ),
            "✅ Companion workspace и официальное сообщество @lavis_userbot настроены."
        );
        assert_eq!(
            provision_completion_text(
                crate::setup_telegram::ProvisionOutcome::CompletedWithoutCommunity(
                    crate::setup_provision::ProvisionError::CommunityJoin,
                ),
                ".",
                crate::i18n::Locale::Russian,
            ),
            "⚠️ Companion workspace готов, но присоединиться к @lavis_userbot не удалось. Повторите .setup repair."
        );
        assert_eq!(
            provision_completion_text(
                crate::setup_telegram::ProvisionOutcome::CompletedWithoutFolder(
                    crate::setup_provision::CompletedWithoutFolder::Capacity,
                ),
                ".",
                crate::i18n::Locale::Russian,
            ),
            "⚠️ Companion workspace настроен без папки: достигнут лимит папок. Повторите .setup repair позже."
        );
        assert_eq!(
            provision_completion_text(
                crate::setup_telegram::ProvisionOutcome::CompletedWithoutFolder(
                    crate::setup_provision::CompletedWithoutFolder::NameOrOwnershipConflict,
                ),
                ".",
                crate::i18n::Locale::Russian,
            ),
            "⚠️ Companion workspace настроен без папки: папка занята или принадлежит другой настройке. Повторите .setup repair после устранения конфликта."
        );
        assert_eq!(
            provision_completion_text(
                crate::setup_telegram::ProvisionOutcome::Failed(
                    crate::setup_grammers::ProvisionError::InviteBot,
                ),
                ".",
                crate::i18n::Locale::Russian,
            ),
            "⚠️ Восстановление companion workspace не завершено. Повторите .setup repair позже."
        );
        assert_eq!(
            provision_completion_text(
                crate::setup_telegram::ProvisionOutcome::Failed(
                    crate::setup_grammers::ProvisionError::InviteBot,
                ),
                ".",
                crate::i18n::Locale::English,
            ),
            "⚠️ Companion workspace repair did not finish. Retry .setup repair later."
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
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "lavis-updates-prefix-{}-{}-{seq}",
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
        let runtime = RuntimeState::new(Instant::now(), aliases, settings);
        assert_eq!(
            route(true, ".help", &runtime),
            Some(Action::Help(crate::commands::HelpRequest::Overview))
        );
        assert_eq!(route(true, ",help", &runtime), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn routes_modules_aliases_and_a_new_prefix_in_the_same_runtime() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "lavis-updates-routing-{}-{}-{seq}",
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
        let mut runtime = RuntimeState::new(Instant::now(), aliases, settings);

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
                contract_revision: None,
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
        let mut runtime = RuntimeState::new(Instant::now(), aliases, settings);
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
        runtime
            .register_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms".to_owned())
            .unwrap();

        assert!(!runtime.consume_expected_self_edit(first_peer, 7, ",ping"));
        assert_eq!(route(true, ",ping", &runtime), Some(Action::Ping));
        assert!(!runtime.consume_expected_self_edit(second_peer, 7, "🏓 Pong: 1 ms"));
        assert_eq!(route(true, ",ping", &runtime), Some(Action::Ping));
        assert!(runtime.consume_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms"));
        assert!(!runtime.consume_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms"));
    }

    /// Scripted [`CommandMediaEdits`] recording every call so the tests can
    /// assert the exact edit sequence without a Telegram connection.
    struct ScriptedMediaEdits {
        photo_results: std::collections::VecDeque<anyhow::Result<()>>,
        text_results: std::collections::VecDeque<Result<(), grammers_client::InvocationError>>,
        photo_calls: Vec<String>,
        text_calls: Vec<String>,
    }

    impl ScriptedMediaEdits {
        fn new(
            photo_results: Vec<anyhow::Result<()>>,
            text_results: Vec<Result<(), grammers_client::InvocationError>>,
        ) -> Self {
            Self {
                photo_results: photo_results.into(),
                text_results: text_results.into(),
                photo_calls: Vec::new(),
                text_calls: Vec::new(),
            }
        }

        fn io_error() -> anyhow::Result<()> {
            Err(anyhow::anyhow!("upload failed"))
        }
    }

    impl CommandMediaEdits for ScriptedMediaEdits {
        async fn edit_photo(
            &mut self,
            _media_url: &str,
            caption: &str,
            _entities: Vec<grammers_client::tl::enums::MessageEntity>,
        ) -> anyhow::Result<()> {
            self.photo_calls.push(caption.to_owned());
            match self.photo_results.pop_front() {
                Some(result) => result,
                None => Err(anyhow::anyhow!("unexpected extra photo call")),
            }
        }

        async fn edit_text(
            &mut self,
            text: &str,
            _entities: Vec<grammers_client::tl::enums::MessageEntity>,
        ) -> Result<(), grammers_client::InvocationError> {
            self.text_calls.push(text.to_owned());
            match self.text_results.pop_front() {
                Some(result) => result,
                None => Err(grammers_client::InvocationError::Io(std::io::Error::other(
                    "unexpected extra text call",
                ))),
            }
        }
    }

    /// Runs the transactional media delivery against a scripted seam.
    async fn deliver(
        runtime: &mut RuntimeState,
        edits: &mut ScriptedMediaEdits,
        message_id: i32,
        media_caption: &str,
        fallback_text: &str,
    ) -> MediaDeliveryOutcome {
        deliver_media_with_suppression(
            edits,
            runtime,
            MediaDeliveryPlan {
                peer_id: PeerId::user(1).unwrap(),
                message_id,
                command: "info",
                media_caption,
                fallback_text,
                fallback_entities: Vec::new(),
                media_url: "https://example.invalid/lavis-info.png",
            },
        )
        .await
    }

    /// `,info` → media response edits the source message → the arriving
    /// MessageEdited matches the registered expectation and is suppressed,
    /// so it never reaches the external module projection.
    #[tokio::test]
    async fn successful_media_delivery_suppresses_its_message_edited_update() {
        let mut runtime = runtime().await;
        let peer = PeerId::user(1).unwrap();
        assert_eq!(
            route(true, ",info", &runtime),
            Some(crate::commands::Action::Info),
            "the flow under test starts at an authenticated ,info command"
        );

        let mut edits = ScriptedMediaEdits::new(vec![Ok(())], vec![]);
        let outcome = deliver(
            &mut runtime,
            &mut edits,
            42,
            "ℹ️ Lavis info card",
            "ℹ️ Lavis info card",
        )
        .await;

        assert_eq!(outcome, MediaDeliveryOutcome::PhotoDelivered);
        assert_eq!(edits.photo_calls, vec!["ℹ️ Lavis info card"]);
        assert!(edits.text_calls.is_empty());

        // The Telegram MessageEdited for the photo edit arrives afterwards;
        // process_update consults this before routing or projecting, so
        // `true` here means zero external message events for that update.
        assert!(runtime.consume_expected_self_edit(peer, 42, "ℹ️ Lavis info card"));
        // Exactly one suppression: a second identical edit is not ours.
        assert!(!runtime.consume_expected_self_edit(peer, 42, "ℹ️ Lavis info card"));
    }

    /// Media delivery fails with an AMBIGUOUS error (request may already have
    /// been applied before the connection broke). The suppression must be
    /// KEPT (fail closed), so a later matching `MessageEdited` is still
    /// suppressed; the text fallback then arms its own expectation.
    #[tokio::test]
    async fn failed_media_delivery_keeps_suppression_and_arms_fallback() {
        let mut runtime = runtime().await;
        let peer = PeerId::user(1).unwrap();

        let mut edits = ScriptedMediaEdits::new(vec![ScriptedMediaEdits::io_error()], vec![Ok(())]);
        let outcome = deliver(
            &mut runtime,
            &mut edits,
            43,
            "ℹ️ media caption",
            "ℹ️ text fallback",
        )
        .await;

        assert_eq!(
            outcome,
            MediaDeliveryOutcome::TextFallbackApplied { delivered: true }
        );
        assert_eq!(edits.photo_calls, vec!["ℹ️ media caption"]);
        assert_eq!(edits.text_calls, vec!["ℹ️ text fallback"]);

        // The media edit is ambiguous: its expectation must STILL be armed so a
        // matching MessageEdited cannot leak into external modules.
        assert!(runtime.consume_expected_self_edit(peer, 43, "ℹ️ media caption"));
        assert!(!runtime.consume_expected_self_edit(peer, 43, "ℹ️ media caption"));
        // The fallback's own expectation suppresses its MessageEdited once.
        assert!(runtime.consume_expected_self_edit(peer, 43, "ℹ️ text fallback"));
        assert!(!runtime.consume_expected_self_edit(peer, 43, "ℹ️ text fallback"));
    }

    /// When media and fallback render the same text, an ambiguous media failure
    /// followed by a definitive fallback rejection leaves exactly one token.
    #[tokio::test]
    async fn ambiguous_media_and_rejected_same_text_fallback_leave_one_token() {
        let mut runtime = runtime().await;
        let peer = PeerId::user(1).unwrap();
        let rpc = grammers_client::InvocationError::Rpc(grammers_mtsender::RpcError {
            code: 400,
            name: "MESSAGE_ID_INVALID".to_owned(),
            value: None,
            caused_by: None,
        });
        let mut edits =
            ScriptedMediaEdits::new(vec![ScriptedMediaEdits::io_error()], vec![Err(rpc)]);

        assert_eq!(
            deliver(&mut runtime, &mut edits, 47, "same text", "same text").await,
            MediaDeliveryOutcome::TextFallbackApplied { delivered: false }
        );
        assert!(runtime.consume_expected_self_edit(peer, 47, "same text"));
        assert!(!runtime.consume_expected_self_edit(peer, 47, "same text"));
    }

    /// When media and fallback render the same text, an ambiguous media failure
    /// followed by a successful fallback preserves both mutation-attempt tokens.
    #[tokio::test]
    async fn ambiguous_media_and_successful_same_text_fallback_leave_two_tokens() {
        let mut runtime = runtime().await;
        let peer = PeerId::user(1).unwrap();
        let mut edits = ScriptedMediaEdits::new(vec![ScriptedMediaEdits::io_error()], vec![Ok(())]);

        assert_eq!(
            deliver(&mut runtime, &mut edits, 48, "same text", "same text").await,
            MediaDeliveryOutcome::TextFallbackApplied { delivered: true }
        );
        assert!(runtime.consume_expected_self_edit(peer, 48, "same text"));
        assert!(runtime.consume_expected_self_edit(peer, 48, "same text"));
        assert!(!runtime.consume_expected_self_edit(peer, 48, "same text"));
    }

    /// Media delivery fails with a DEFINITIVE server rejection (`Rpc`): the
    /// mutation provably was not applied, so its suppression is released and
    /// the text fallback arms only its own expectation.
    #[tokio::test]
    async fn definitively_rejected_media_delivery_releases_suppression() {
        let mut runtime = runtime().await;
        let peer = PeerId::user(1).unwrap();

        let media_rejection = anyhow::anyhow!(grammers_client::InvocationError::Rpc(
            grammers_mtsender::RpcError {
                code: 400,
                name: "MESSAGE_ID_INVALID".to_owned(),
                value: None,
                caused_by: None,
            }
        ));
        let mut edits = ScriptedMediaEdits::new(vec![Err(media_rejection)], vec![Ok(())]);
        let outcome = deliver(
            &mut runtime,
            &mut edits,
            46,
            "ℹ️ media caption",
            "ℹ️ text fallback",
        )
        .await;

        assert_eq!(
            outcome,
            MediaDeliveryOutcome::TextFallbackApplied { delivered: true }
        );
        // The media edit was definitively rejected: no MessageEdited will ever
        // arrive for it, so its stale expectation must be gone.
        assert!(!runtime.consume_expected_self_edit(peer, 46, "ℹ️ media caption"));
        // The fallback's own expectation suppresses its MessageEdited once.
        assert!(runtime.consume_expected_self_edit(peer, 46, "ℹ️ text fallback"));
        assert!(!runtime.consume_expected_self_edit(peer, 46, "ℹ️ text fallback"));
    }

    #[tokio::test]
    async fn wrapped_media_invocation_error_remains_classifiable_as_rpc() {
        let mut runtime = runtime().await;
        let peer = PeerId::user(1).unwrap();
        let media_rejection = anyhow::anyhow!(grammers_client::InvocationError::Rpc(
            grammers_mtsender::RpcError {
                code: 400,
                name: "MESSAGE_ID_INVALID".to_owned(),
                value: None,
                caused_by: None,
            }
        ))
        .context("edit message with photo");
        let mut edits = ScriptedMediaEdits::new(vec![Err(media_rejection)], vec![Ok(())]);

        deliver(&mut runtime, &mut edits, 49, "media", "fallback").await;

        assert!(!runtime.consume_expected_self_edit(peer, 49, "media"));
        assert!(runtime.consume_expected_self_edit(peer, 49, "fallback"));
    }

    /// Both edits failing with AMBIGUOUS errors must leave BOTH suppressions
    /// armed (fail closed): the mutation may have applied despite the error, so
    /// neither a media nor a text `MessageEdited` may leak.
    #[tokio::test]
    async fn double_failure_keeps_all_suppressions() {
        let mut runtime = runtime().await;
        let peer = PeerId::user(1).unwrap();

        let mut edits = ScriptedMediaEdits::new(
            vec![ScriptedMediaEdits::io_error()],
            vec![Err(grammers_client::InvocationError::Io(
                std::io::Error::other("edit rejected"),
            ))],
        );
        let outcome = deliver(
            &mut runtime,
            &mut edits,
            44,
            "ℹ️ media caption",
            "ℹ️ text fallback",
        )
        .await;

        assert_eq!(
            outcome,
            MediaDeliveryOutcome::TextFallbackApplied { delivered: false }
        );
        // Both edits are ambiguous → both suppressions stay armed.
        assert!(runtime.consume_expected_self_edit(peer, 44, "ℹ️ media caption"));
        assert!(runtime.consume_expected_self_edit(peer, 44, "ℹ️ text fallback"));
        assert!(!runtime.consume_expected_self_edit(peer, 44, "ℹ️ media caption"));
        assert!(!runtime.consume_expected_self_edit(peer, 44, "ℹ️ text fallback"));
    }

    /// A foreign edit of the same message id must not be swallowed by a
    /// pending media suppression entry: content, peer, and id all bind.
    #[tokio::test]
    async fn foreign_edited_update_is_not_masked_by_media_suppression() {
        let mut runtime = runtime().await;
        let peer = PeerId::user(1).unwrap();

        let mut edits = ScriptedMediaEdits::new(vec![Ok(())], vec![]);
        deliver(
            &mut runtime,
            &mut edits,
            45,
            "ℹ️ media caption",
            "ℹ️ media caption",
        )
        .await;

        assert!(!runtime.consume_expected_self_edit(peer, 45, "user typed this themselves"));
        assert!(
            runtime.consume_expected_self_edit(peer, 45, "ℹ️ media caption"),
            "the genuine media caption still suppresses"
        );
    }

    /// Only a definitive server `Rpc` rejection proves the edit was not applied
    /// and may release its suppression. Ambiguous transport/read/session errors
    /// (the request may have been applied before the confirmation was lost) must
    /// fail closed and keep it.
    #[test]
    fn edit_error_classification_fails_closed_on_ambiguous_errors() {
        fn rpc(code: i32, name: &str) -> grammers_client::InvocationError {
            grammers_client::InvocationError::Rpc(grammers_mtsender::RpcError {
                code,
                name: name.to_owned(),
                value: None,
                caused_by: None,
            })
        }

        let io = grammers_client::InvocationError::Io(std::io::Error::other("read failed"));
        let dropped = grammers_client::InvocationError::Dropped;
        let invalid_dc = grammers_client::InvocationError::InvalidDc;

        // Ambiguous: never proof the mutation did not happen.
        assert!(!edit_definitely_rejected(&io));
        assert!(!edit_definitely_rejected(&dropped));
        assert!(!edit_definitely_rejected(&invalid_dc));

        // Definitive server rejection: safe to release the suppression because
        // no MessageEdited can follow.
        assert!(edit_definitely_rejected(&rpc(400, "MESSAGE_ID_INVALID")));
        assert!(edit_definitely_rejected(&rpc(400, "MESSAGE_NOT_MODIFIED")));
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

    struct TestResolver {
        main: Option<String>,
        calls: Arc<AtomicUsize>,
    }

    impl UpstreamRev for TestResolver {
        fn main_rev<'a>(&'a self) -> UpstreamRevFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let main = self.main.clone();
            Box::pin(async move {
                match main {
                    Some(revision) => Ok(revision),
                    None => std::future::pending().await,
                }
            })
        }

        fn compare<'a>(&'a self, _base: &'a str, _head: &'a str) -> CompareFuture<'a> {
            panic!("compare must not be called by this refresh test")
        }
    }

    #[tokio::test]
    async fn upstream_refresh_is_immediate() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(upstream_refresh_task_with(
            Box::new(TestResolver {
                main: Some(crate::info::build_rev().to_owned()),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            sender,
            Duration::from_secs(3600),
            Duration::from_secs(1),
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(100), receiver.recv())
                .await
                .unwrap(),
            Some(UpstreamRefresh::Success(_))
        ));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn upstream_refresh_deadline_publishes_timeout_failure() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(upstream_refresh_task_with(
            Box::new(TestResolver {
                main: None,
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            sender,
            Duration::from_secs(3600),
            Duration::from_millis(10),
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(100), receiver.recv())
                .await
                .unwrap(),
            Some(UpstreamRefresh::Failed(category)) if category == "timeout"
        ));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn closed_refresh_receiver_disables_receive_branch() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(sender);
        let mut receiver = Some(&mut receiver);
        assert!(receive_refresh(&mut receiver).await.is_none());
        receiver = None;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), receive_refresh(&mut receiver))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn refresh_task_cancel_and_join_stops_never_completing_refresh() {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let task = RefreshTask {
            handle: Some(tokio::spawn(upstream_refresh_task_with(
                Box::new(TestResolver {
                    main: None,
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
                sender,
                Duration::from_secs(3600),
                Duration::from_secs(3600),
            ))),
        };
        task.cancel_and_join().await;
    }

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Future for DropSignal {
        type Output = ();

        fn poll(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            std::task::Poll::Pending
        }
    }

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn dropping_refresh_task_aborts_in_flight_work() {
        let (dropped_sender, dropped_receiver) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(DropSignal(Some(dropped_sender)));
        drop(RefreshTask {
            handle: Some(handle),
        });
        tokio::time::timeout(Duration::from_millis(100), dropped_receiver)
            .await
            .unwrap()
            .unwrap();
    }
}
