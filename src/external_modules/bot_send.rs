//! Companion-bot post boundary for host.invoke `message.sendBot`,
//! `inline.form`, `inline.answer`, and `message.editBot`.
//!
//! The module never sees bot credentials: it names a destination and text,
//! and this sender loads the companion token from the local setup store and
//! posts through the Bot API (or bridges the inline bot through the user
//! session). Error strings are static so no token material or HTTP detail
//! can leak into module-visible diagnostics.

use super::v6_host::{HostBotSend, HostInlineSurface, InlineButton, peer_id_from_bot_chat_id};
use crate::bot_api::{BotEditMessage, BotMessage, BotSendApi, HttpBotApi};
use crate::setup_store::SetupStore;
use grammers_client::tl;
use grammers_session::Session;
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerId;
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub struct CompanionBotSender {
    state_path: PathBuf,
    token_path: PathBuf,
    api: HttpBotApi,
    client: grammers_client::Client,
    session: Arc<SqliteSession>,
    registry: Arc<InlineMenuRegistry>,
    publications: MenuPublications,
    bot_form: crate::message_provenance::SharedBotFormLedger,
}

/// Remembers the (peer, message id) of each module's most recently published
/// menus. Bot API callbacks for via-bot inline messages carry only an
/// `inline_message_id` (no `query.message`), and the host cannot convert it
/// to a chat/message pair, so a Close press is correlated to the newest live
/// publication of that module. LIFO: menus are closed shortly after opening.
#[derive(Default)]
struct MenuPublications {
    entries: Mutex<HashMap<String, VecDeque<(PeerId, i32)>>>,
}

const MENU_PUBLICATIONS_CAP: usize = 4;

impl MenuPublications {
    fn record(&self, module_id: &str, peer: PeerId, message_id: i32) {
        let mut entries = self.lock_entries();
        let ring = entries.entry(module_id.to_owned()).or_default();
        if ring.len() >= MENU_PUBLICATIONS_CAP {
            ring.pop_front();
        }
        ring.push_back((peer, message_id));
    }

    fn take_latest(&self, module_id: &str) -> Option<(PeerId, i32)> {
        let mut entries = self.lock_entries();
        entries.get_mut(module_id)?.pop_back()
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, VecDeque<(PeerId, i32)>>> {
        match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// The module owning a publication, taken from the callback-data namespace.
/// `V6HostExecutor::data_prefix` builds the prefix as exactly
/// `format!("{module_id}|")`, so stripping the trailing separator recovers
/// the module id; anything else is a caller contract violation.
fn module_id_from_data_prefix(prefix: &str) -> Option<&str> {
    let module_id = prefix.strip_suffix('|')?;
    if module_id.is_empty() || module_id.contains('|') {
        None
    } else {
        Some(module_id)
    }
}

/// Logs the sanitized Telegram rejection name (constant identifiers only —
/// never tokens or URLs) so menu failures are diagnosable from the Logs
/// topic and journalctl without exposing anything else.
fn log_inline_rejection(error: &grammers_client::InvocationError, stage: &str) {
    match error {
        grammers_client::InvocationError::Rpc(rpc) => {
            tracing::warn!(
                target: "lavis_inline_form",
                stage,
                name = %rpc.name,
                "inline query rejected by Telegram"
            );
        }
        other => {
            tracing::warn!(
                target: "lavis_inline_form",
                stage,
                error = %other,
                "inline query failed"
            );
        }
    }
}

impl CompanionBotSender {
    pub fn new(
        state_path: PathBuf,
        token_path: PathBuf,
        client: grammers_client::Client,
        session: Arc<SqliteSession>,
        bot_form: crate::message_provenance::SharedBotFormLedger,
    ) -> Result<Self, crate::bot_api::BotApiError> {
        Ok(Self {
            state_path,
            token_path,
            api: HttpBotApi::new()?,
            client,
            session,
            registry: Arc::new(InlineMenuRegistry::default()),
            publications: MenuPublications::default(),
            bot_form,
        })
    }

    /// The registry shared with the getUpdates poller: forms published here
    /// are answered from there. Both sides must hold the same instance.
    pub fn registry(&self) -> Arc<InlineMenuRegistry> {
        self.registry.clone()
    }

    fn sanitized_error(error: crate::bot_api::BotApiError) -> String {
        match error {
            crate::bot_api::BotApiError::Rejected => "bot send rejected".to_owned(),
            crate::bot_api::BotApiError::Timeout => "bot send timeout".to_owned(),
            _ => "bot send unavailable".to_owned(),
        }
    }

    async fn load_token(&self) -> Result<crate::setup_store::CompanionToken, String> {
        let state_path = self.state_path.clone();
        let token_path = self.token_path.clone();
        tokio::task::spawn_blocking(move || SetupStore::new(state_path, token_path).load_token())
            .await
            .map_err(|_| "bot send unavailable".to_owned())?
            .map_err(|_| "companion bot is not configured".to_owned())
    }

    async fn load_bot_username(&self) -> Result<String, String> {
        let state_path = self.state_path.clone();
        let token_path = self.token_path.clone();
        let state = tokio::task::spawn_blocking(move || {
            SetupStore::new(state_path, token_path).load_state()
        })
        .await
        .map_err(|_| "inline form unavailable".to_owned())?
        .map_err(|_| "companion bot is not configured".to_owned())?;
        state
            .identities
            .bot_username
            .filter(|username| !username.is_empty())
            .ok_or_else(|| "companion bot is not configured".to_owned())
    }

    const INLINE_CALL_TIMEOUT: Duration = Duration::from_secs(10);

    async fn send_inline_form(
        &self,
        destination_peer: tl::enums::InputPeer,
        query: &str,
    ) -> Result<Vec<(PeerId, i32)>, String> {
        let username = self.load_bot_username().await?;
        let bot_peer = tokio::time::timeout(
            Self::INLINE_CALL_TIMEOUT,
            self.client.resolve_username(&username),
        )
        .await
        .map_err(|_| "inline form timeout".to_owned())?
        .map_err(|error| {
            log_inline_rejection(&error, "resolve_bot");
            "inline form rejected".to_owned()
        })?;
        let bot = match bot_peer {
            Some(grammers_client::peer::Peer::User(user)) => match user.raw {
                tl::enums::User::User(user) => {
                    let access_hash = user
                        .access_hash
                        .ok_or_else(|| "companion bot is not configured".to_owned())?;
                    tl::enums::InputUser::User(tl::types::InputUser {
                        user_id: user.id,
                        access_hash,
                    })
                }
                tl::enums::User::Empty(_) => {
                    return Err("companion bot is not configured".to_owned());
                }
            },
            _ => return Err("companion bot is not configured".to_owned()),
        };
        // Telegram races the companion bot's answer on fresh queries, so a
        // retry gives slow answers a second chance.
        let mut last_error = "inline form rejected".to_owned();
        let mut result_id = String::new();
        let mut query_id = 0i64;
        for attempt in 0..2u32 {
            let response = match tokio::time::timeout(
                Self::INLINE_CALL_TIMEOUT,
                self.client
                    .invoke(&tl::functions::messages::GetInlineBotResults {
                        bot: bot.clone(),
                        peer: destination_peer.clone(),
                        geo_point: None,
                        query: query.to_owned(),
                        offset: String::new(),
                    }),
            )
            .await
            {
                Ok(response) => response,
                Err(_) => {
                    last_error = "inline form timeout".to_owned();
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    continue;
                }
            };
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    log_inline_rejection(&error, "get_inline_results");
                    last_error = "inline form rejected".to_owned();
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    continue;
                }
            };
            let tl::enums::messages::BotResults::Results(results) = response;
            query_id = results.query_id;
            let Some(first) = results.results.first() else {
                // Telegram races the companion bot's answer on fresh queries.
                tracing::warn!(
                    target: "lavis_inline_form",
                    stage = "get_inline_results",
                    attempt,
                    "inline query returned no results"
                );
                last_error = "inline form rejected (no results)".to_owned();
                tokio::time::sleep(Duration::from_millis(400)).await;
                continue;
            };
            result_id = match first {
                tl::enums::BotInlineResult::Result(result) => result.id.clone(),
                tl::enums::BotInlineResult::BotInlineMediaResult(result) => result.id.clone(),
            };
            break;
        }
        if result_id.is_empty() {
            return Err(last_error);
        }
        let mut random_id = [0u8; 8];
        getrandom::fill(&mut random_id).map_err(|_| "inline form unavailable".to_owned())?;
        let updates = tokio::time::timeout(
            Self::INLINE_CALL_TIMEOUT,
            self.client
                .invoke(&tl::functions::messages::SendInlineBotResult {
                    silent: false,
                    background: false,
                    clear_draft: false,
                    hide_via: false,
                    peer: destination_peer,
                    reply_to: None,
                    random_id: i64::from_le_bytes(random_id),
                    query_id,
                    id: result_id,
                    schedule_date: None,
                    send_as: None,
                    quick_reply_shortcut: None,
                    allow_paid_stars: None,
                }),
        )
        .await
        .map_err(|_| "inline form timeout".to_owned())?
        .map_err(|error| {
            log_inline_rejection(&error, "send_inline_result");
            "inline form rejected".to_owned()
        })?;
        // The via-bot menu is a self-authored update whose text is
        // module-controlled; register it so the runtime consumes the update
        // before command routing sees it.
        let published = Self::extract_message_ids(&updates);
        for (peer_id, message_id) in &published {
            self.bot_form.register(*peer_id, *message_id);
        }
        Ok(published)
    }

    fn extract_message_ids(updates: &tl::enums::Updates) -> Vec<(PeerId, i32)> {
        let list = match updates {
            tl::enums::Updates::Updates(u) => &u.updates,
            tl::enums::Updates::Combined(u) => &u.updates,
            _ => return Vec::new(),
        };
        list.iter()
            .filter_map(|update| {
                let message = match update {
                    tl::enums::Update::NewMessage(wrapped) => &wrapped.message,
                    tl::enums::Update::NewChannelMessage(wrapped) => &wrapped.message,
                    _ => return None,
                };
                let message = match message {
                    tl::enums::Message::Message(message) => message,
                    _ => return None,
                };
                let peer_id = match &message.peer_id {
                    tl::enums::Peer::User(user) => PeerId::user(user.user_id)?,
                    tl::enums::Peer::Chat(chat) => PeerId::chat(chat.chat_id)?,
                    tl::enums::Peer::Channel(channel) => PeerId::channel(channel.channel_id)?,
                };
                Some((peer_id, message.id))
            })
            .collect()
    }

    fn prefixed_rows(
        buttons: &[Vec<InlineButton>],
        data_prefix: &str,
    ) -> Vec<Vec<(String, String)>> {
        buttons
            .iter()
            .map(|row| {
                row.iter()
                    .map(|button| (button.text.clone(), format!("{data_prefix}{}", button.data)))
                    .collect()
            })
            .collect()
    }
}

impl HostBotSend for CompanionBotSender {
    fn send<'a>(
        &'a self,
        chat_id: i64,
        message_thread_id: Option<i32>,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let token = self.load_token().await?;
            let message = BotMessage {
                chat_id,
                message_thread_id,
                text: text.to_owned(),
            };
            self.api
                .send_message(&token, &message)
                .await
                .map_err(Self::sanitized_error)
        })
    }
}

impl HostInlineSurface for CompanionBotSender {
    fn form<'a>(
        &'a self,
        input_peer: tl::enums::InputPeer,
        text: &'a str,
        buttons: &'a [Vec<InlineButton>],
        data_prefix: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let rows = Self::prefixed_rows(buttons, data_prefix);
            let mut query_bytes = [0u8; 16];
            getrandom::fill(&mut query_bytes).map_err(|_| "inline form unavailable".to_owned())?;
            let query: String = query_bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            self.registry.insert(
                query.clone(),
                InlineMenu {
                    text: text.to_owned(),
                    rows,
                },
            );
            match self.send_inline_form(input_peer, &query).await {
                Ok(published) => {
                    match module_id_from_data_prefix(data_prefix) {
                        Some(module_id) => {
                            for (peer, message_id) in published {
                                self.publications.record(module_id, peer, message_id);
                            }
                        }
                        None => {
                            tracing::warn!(
                                target: "lavis_inline_form",
                                stage = "record_publication",
                                "inline form data prefix does not name a module; close correlation unavailable"
                            );
                        }
                    }
                    Ok(())
                }
                Err(error) => {
                    self.registry.take(&query);
                    Err(error)
                }
            }
        })
    }

    fn answer<'a>(
        &'a self,
        callback_id: &'a str,
        text: &'a str,
        show_alert: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let token = self
                .load_token()
                .await
                .map_err(|message| match message.as_str() {
                    "bot send unavailable" => "callback answer unavailable".to_owned(),
                    other => other.to_owned(),
                })?;
            self.api
                .answer_callback(&token, callback_id, text, show_alert)
                .await
                .map_err(|error| match error {
                    crate::bot_api::BotApiError::Rejected => "callback answer rejected".to_owned(),
                    crate::bot_api::BotApiError::Timeout => "callback answer timeout".to_owned(),
                    _ => "callback answer unavailable".to_owned(),
                })
        })
    }

    fn edit<'a>(
        &'a self,
        chat_id: i64,
        message_id: i64,
        inline_message_id: &'a str,
        text: &'a str,
        buttons: &'a [Vec<InlineButton>],
        data_prefix: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let token = self
                .load_token()
                .await
                .map_err(|message| match message.as_str() {
                    "bot send unavailable" => "bot edit unavailable".to_owned(),
                    other => other.to_owned(),
                })?;
            let message = BotEditMessage {
                chat_id,
                message_id,
                inline_message_id: if inline_message_id.is_empty() {
                    None
                } else {
                    Some(inline_message_id.to_owned())
                },
                text: text.to_owned(),
                buttons: Self::prefixed_rows(buttons, data_prefix),
            };
            self.api
                .edit_message_text(&token, &message)
                .await
                .map_err(|error| match error {
                    crate::bot_api::BotApiError::Rejected => "bot edit rejected".to_owned(),
                    crate::bot_api::BotApiError::Timeout => "bot edit timeout".to_owned(),
                    _ => "bot edit unavailable".to_owned(),
                })
        })
    }

    fn delete_bot_message<'a>(
        &'a self,
        chat_id: i64,
        message_id: i64,
        module_id: &'a str,
        inline_message_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            // The USER session deletes via-bot messages from its own dialogs
            // unconditionally — the Bot API cannot address Saved Messages and
            // needs moderation rights in groups, so it was the wrong tool.
            let peer = if chat_id != 0 && message_id > 0 {
                match peer_id_from_bot_chat_id(chat_id) {
                    Some(peer) => (peer, message_id as i32),
                    None => return Err("bot delete unavailable".to_owned()),
                }
            } else if !inline_message_id.is_empty() {
                // Bot API callbacks for via-bot inline messages carry no
                // `query.message`, only `inline_message_id`, and Telegram
                // exposes no conversion to a chat/message pair. Correlate
                // against the menus this host published for the module.
                tracing::info!(
                    target: "lavis_inline_form",
                    stage = "delete_menu_inline",
                    module_id,
                    "deleting menu via publication correlation"
                );
                match self.publications.take_latest(module_id) {
                    Some((peer, message_id)) => (peer, message_id),
                    None => {
                        tracing::warn!(
                            target: "lavis_inline_form",
                            stage = "delete_menu_no_record",
                            module_id,
                            "no recorded publication to correlate the close press with"
                        );
                        return Err("bot delete rejected".to_owned());
                    }
                }
            } else {
                return Err("bot delete unavailable".to_owned());
            };
            self.delete_by_peer(peer.0, peer.1).await
        })
    }
}

impl CompanionBotSender {
    /// Deletes a single message by MTProto peer: session-backed access-hash
    /// resolution first, zero-auth network resolve as fallback for peers the
    /// session has never seen (basic chats).
    async fn delete_by_peer(&self, peer: PeerId, message_id: i32) -> Result<(), String> {
        let session_ref = match self.session.peer_ref(peer).await {
            Ok(peer_ref) => peer_ref,
            Err(error) => {
                tracing::warn!(
                    target: "lavis_inline_form",
                    stage = "resolve_menu_session",
                    chat_id = peer.bare_id_unchecked(),
                    error = %error,
                    "session peer lookup failed, falling back to network resolve"
                );
                None
            }
        };
        if let Some(peer_ref) = session_ref {
            return match tokio::time::timeout(
                Self::INLINE_CALL_TIMEOUT,
                self.client.delete_messages(peer_ref, &[message_id]),
            )
            .await
            {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(error)) => {
                    log_inline_rejection(&error, "delete_menu");
                    Err("bot delete rejected".to_owned())
                }
                Err(_) => {
                    tracing::warn!(
                        target: "lavis_inline_form",
                        stage = "delete_menu_timeout",
                        "menu delete timed out"
                    );
                    Err("bot delete timeout".to_owned())
                }
            };
        }
        let reference = grammers_session::types::PeerRef {
            id: peer,
            auth: grammers_session::types::PeerAuth::default(),
        };
        let resolved = match tokio::time::timeout(
            Self::INLINE_CALL_TIMEOUT,
            self.client.resolve_peer(reference),
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(_) => {
                tracing::warn!(
                    target: "lavis_inline_form",
                    stage = "resolve_menu_timeout",
                    chat_id = peer.bare_id_unchecked(),
                    "menu resolve timed out"
                );
                return Err("bot delete timeout".to_owned());
            }
        }
        .map_err(|error| {
            tracing::warn!(
                target: "lavis_inline_form",
                stage = "resolve_menu",
                error = %error,
                "Could not resolve the menu chat"
            );
            "bot delete rejected".to_owned()
        })?;
        let peer_ref = tokio::time::timeout(Self::INLINE_CALL_TIMEOUT, resolved.to_ref())
            .await
            .map_err(|_| {
                tracing::warn!(
                    target: "lavis_inline_form",
                    stage = "resolve_menu_ref_timeout",
                    chat_id = peer.bare_id_unchecked(),
                    "menu reference lookup timed out"
                );
                "bot delete timeout".to_owned()
            })?
            .map_err(|error| {
                tracing::warn!(
                    target: "lavis_inline_form",
                    stage = "resolve_menu_ref",
                    error = %error,
                    "Could not reference the menu chat"
                );
                "bot delete rejected".to_owned()
            })?
            .ok_or_else(|| {
                tracing::warn!(
                    target: "lavis_inline_form",
                    stage = "resolve_menu_ref",
                    chat_id = peer.bare_id_unchecked(),
                    "menu chat resolved to no reference"
                );
                "bot delete rejected".to_owned()
            })?;
        tokio::time::timeout(
            Self::INLINE_CALL_TIMEOUT,
            self.client.delete_messages(peer_ref, &[message_id]),
        )
        .await
        .map_err(|_| "bot delete timeout".to_owned())?
        .map_err(|error| {
            log_inline_rejection(&error, "delete_menu");
            "bot delete rejected".to_owned()
        })?;
        Ok(())
    }
}

/// A menu staged by `inline.form`, waiting for the companion bot's inline
/// query so it can be answered with buttons whose callback data is already
/// host-namespaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineMenu {
    pub text: String,
    pub rows: Vec<Vec<(String, String)>>,
}

const INLINE_MENU_TTL: Duration = Duration::from_secs(120);
const INLINE_MENU_CAP: usize = 64;

/// Maps pending inline queries to staged menus. Entries expire after
/// [`INLINE_MENU_TTL`] and the map is capped so a misbehaving module cannot
/// grow it without bound.
#[derive(Default)]
pub struct InlineMenuRegistry {
    entries: Mutex<HashMap<String, (InlineMenu, Instant)>>,
}

impl InlineMenuRegistry {
    pub fn insert(&self, query: String, menu: InlineMenu) {
        let mut entries = self.lock_entries();
        entries.retain(|_, (_, inserted)| inserted.elapsed() < INLINE_MENU_TTL);
        if entries.len() >= INLINE_MENU_CAP
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (_, inserted))| *inserted)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(query, (menu, Instant::now()));
    }

    pub fn take(&self, query: &str) -> Option<InlineMenu> {
        let mut entries = self.lock_entries();
        entries
            .remove(query)
            .filter(|(_, inserted)| inserted.elapsed() < INLINE_MENU_TTL)
            .map(|(menu, _)| menu)
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, (InlineMenu, Instant)>> {
        match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Test double mirroring the sanitized error surface of the real sender.
pub struct StaticBotSender {
    pub fail: Option<String>,
}

impl HostBotSend for StaticBotSender {
    fn send<'a>(
        &'a self,
        _chat_id: i64,
        _message_thread_id: Option<i32>,
        _text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move { self.fail.clone().map_or(Ok(()), Err) })
    }
}

pub fn arc_sender(sender: &Arc<CompanionBotSender>) -> Arc<dyn HostBotSend> {
    sender.clone()
}

pub fn arc_inline(sender: &Arc<CompanionBotSender>) -> Arc<dyn HostInlineSurface> {
    sender.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu(text: &str) -> InlineMenu {
        InlineMenu {
            text: text.to_owned(),
            rows: vec![vec![("ok".to_owned(), "m|data".to_owned())]],
        }
    }

    #[test]
    fn registry_round_trip() {
        let registry = InlineMenuRegistry::default();
        registry.insert("q1".into(), menu("hello"));
        assert_eq!(registry.take("q1"), Some(menu("hello")));
        assert_eq!(registry.take("q1"), None);
        assert_eq!(registry.take("missing"), None);
    }

    #[test]
    fn registry_is_bounded() {
        let registry = InlineMenuRegistry::default();
        for index in 0..(INLINE_MENU_CAP + 16) {
            registry.insert(format!("q{index}"), menu("x"));
        }
        assert!(registry.take("q0").is_none());
        let last = INLINE_MENU_CAP + 15;
        assert!(registry.take(&format!("q{last}")).is_some());
    }

    #[test]
    fn registry_entries_expire() {
        let registry = InlineMenuRegistry::default();
        registry.insert("stale".into(), menu("x"));
        {
            let mut entries = registry.lock_entries();
            entries.insert(
                "stale".into(),
                (
                    menu("x"),
                    Instant::now() - INLINE_MENU_TTL - Duration::from_secs(1),
                ),
            );
        }
        assert_eq!(registry.take("stale"), None);
    }

    #[test]
    fn publications_are_lifo_per_module() {
        let publications = MenuPublications::default();
        let first = PeerId::chat(1).unwrap();
        let second = PeerId::chat(2).unwrap();
        publications.record("z", first, 11);
        publications.record("z", second, 22);
        assert_eq!(publications.take_latest("z"), Some((second, 22)));
        assert_eq!(publications.take_latest("z"), Some((first, 11)));
        assert_eq!(publications.take_latest("z"), None);
        assert_eq!(publications.take_latest("other"), None);
    }

    #[test]
    fn publications_ring_drops_oldest_at_cap() {
        let publications = MenuPublications::default();
        for index in 0..(MENU_PUBLICATIONS_CAP as i32 + 2) {
            publications.record("z", PeerId::chat(1).unwrap(), 100 + index);
        }
        assert_eq!(
            publications.take_latest("z"),
            Some((
                PeerId::chat(1).unwrap(),
                100 + MENU_PUBLICATIONS_CAP as i32 + 1
            ))
        );
        let mut remaining = Vec::new();
        while let Some((_, message_id)) = publications.take_latest("z") {
            remaining.push(message_id);
        }
        // The two oldest entries (100, 101) were evicted.
        assert_eq!(remaining, vec![104, 103, 102]);
    }

    #[test]
    fn module_id_from_data_prefix_expects_single_namespace() {
        assert_eq!(module_id_from_data_prefix("z|"), Some("z"));
        assert_eq!(module_id_from_data_prefix("cleaner|"), Some("cleaner"));
        assert_eq!(module_id_from_data_prefix("|"), None);
        assert_eq!(module_id_from_data_prefix("z"), None);
        assert_eq!(module_id_from_data_prefix("a|b|"), None);
    }
}
