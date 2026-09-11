//! Companion-bot post boundary for host.invoke `message.sendBot`,
//! `inline.form`, `inline.answer`, and `message.editBot`.
//!
//! The module never sees bot credentials: it names a destination and text,
//! and this sender loads the companion token from the local setup store and
//! posts through the Bot API (or bridges the inline bot through the user
//! session). Error strings are static so no token material or HTTP detail
//! can leak into module-visible diagnostics.

use super::v6_host::{HostBotSend, HostInlineSurface, InlineButton};
use crate::bot_api::{BotEditMessage, BotMessage, BotSendApi, HttpBotApi};
use crate::setup_store::SetupStore;
use grammers_client::tl;
use grammers_session::types::PeerId;
use std::{
    collections::HashMap,
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
    registry: Arc<InlineMenuRegistry>,
    bot_form: crate::message_provenance::SharedBotFormLedger,
}

impl CompanionBotSender {
    pub fn new(
        state_path: PathBuf,
        token_path: PathBuf,
        client: grammers_client::Client,
        bot_form: crate::message_provenance::SharedBotFormLedger,
    ) -> Result<Self, crate::bot_api::BotApiError> {
        Ok(Self {
            state_path,
            token_path,
            api: HttpBotApi::new()?,
            client,
            registry: Arc::new(InlineMenuRegistry::default()),
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

    async fn send_inline_form(&self, peer: PeerId, query: &str) -> Result<(), String> {
        let username = self.load_bot_username().await?;
        let bot_peer = tokio::time::timeout(
            Self::INLINE_CALL_TIMEOUT,
            self.client.resolve_username(&username),
        )
        .await
        .map_err(|_| "inline form timeout".to_owned())?
        .map_err(|_| "inline form rejected".to_owned())?;
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
        // PeerId carries no access hash; resolve through the session so the
        // destination uses a real access hash (ambient authority fails with
        // CHANNEL_INVALID outside the self chat).
        let reference = grammers_session::types::PeerRef {
            id: peer,
            auth: grammers_session::types::PeerAuth::default(),
        };
        let resolved = tokio::time::timeout(
            Self::INLINE_CALL_TIMEOUT,
            self.client.resolve_peer(reference),
        )
        .await
        .map_err(|_| "inline form timeout".to_owned())?
        .map_err(|_| "inline form rejected".to_owned())?;
        let peer_ref = tokio::time::timeout(Self::INLINE_CALL_TIMEOUT, resolved.to_ref())
            .await
            .map_err(|_| "inline form timeout".to_owned())?
            .map_err(|_| "inline form rejected".to_owned())?
            .ok_or_else(|| "inline form rejected".to_owned())?;
        let destination_peer = tl::enums::InputPeer::from(&peer_ref);

        let response = tokio::time::timeout(
            Self::INLINE_CALL_TIMEOUT,
            self.client
                .invoke(&tl::functions::messages::GetInlineBotResults {
                    bot,
                    peer: destination_peer.clone(),
                    geo_point: None,
                    query: query.to_owned(),
                    offset: String::new(),
                }),
        )
        .await
        .map_err(|_| "inline form timeout".to_owned())?
        .map_err(|_| "inline form rejected".to_owned())?;
        let tl::enums::messages::BotResults::Results(results) = response;
        let result_id = match results.results.first() {
            Some(tl::enums::BotInlineResult::Result(result)) => result.id.clone(),
            Some(tl::enums::BotInlineResult::BotInlineMediaResult(result)) => result.id.clone(),
            None => return Err("inline form rejected".to_owned()),
        };
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
                    query_id: results.query_id,
                    id: result_id,
                    schedule_date: None,
                    send_as: None,
                    quick_reply_shortcut: None,
                    allow_paid_stars: None,
                }),
        )
        .await
        .map_err(|_| "inline form timeout".to_owned())?
        .map_err(|_| "inline form rejected".to_owned())?;
        // The via-bot menu is a self-authored update whose text is
        // module-controlled; register it so the runtime consumes the update
        // before command routing sees it.
        for (peer_id, message_id) in Self::extract_message_ids(&updates) {
            self.bot_form.register(peer_id, message_id);
        }
        Ok(())
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
        peer: PeerId,
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
            match self.send_inline_form(peer, &query).await {
                Ok(()) => Ok(()),
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
}
