use super::v6_handles::{V6HandleKind, V6HandleRegistry};
use crate::message_provenance::SharedSelfEditLedger;
use crate::message_provenance::edit_definitely_rejected;
use grammers_session::types::PeerId;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MessageEditParams {
    pub message: String,
    pub text: String,
}

impl MessageEditParams {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.message.is_empty() {
            return Err("message handle is required");
        }
        if self.text.is_empty() {
            return Err("text is required");
        }
        if self.text.contains('\0') {
            return Err("text contains NUL");
        }
        if self.text.encode_utf16().count() > 4096 {
            return Err("text is too long");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MessageSendBotParams {
    pub chat_id: i64,
    #[serde(default)]
    pub message_thread_id: Option<i32>,
    pub text: String,
}

impl MessageSendBotParams {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.chat_id == 0 {
            return Err("chat_id is required");
        }
        if self.message_thread_id.is_some_and(|thread| thread <= 0) {
            return Err("message_thread_id must be positive");
        }
        if self.text.is_empty() {
            return Err("text is required");
        }
        if self.text.contains('\0') {
            return Err("text contains NUL");
        }
        if self.text.encode_utf16().count() > 4096 {
            return Err("text is too long");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InlineFormParams {
    pub peer: String,
    pub text: String,
    #[serde(default)]
    pub buttons: Vec<Vec<InlineButton>>,
}

impl InlineFormParams {
    pub fn validate(&self, prefix_len: usize) -> Result<(), &'static str> {
        if self.peer.is_empty() {
            return Err("peer handle is required");
        }
        if self.text.is_empty() || self.text.contains('\0') {
            return Err("text is required");
        }
        if self.text.encode_utf16().count() > 4096 {
            return Err("text is too long");
        }
        validate_button_rows(&self.buttons, prefix_len)
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InlineAnswerParams {
    pub callback_id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub show_alert: bool,
}

impl InlineAnswerParams {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.callback_id.is_empty() || self.callback_id.len() > 128 {
            return Err("callback_id is required");
        }
        if self.text.contains('\0') || self.text.encode_utf16().count() > 200 {
            return Err("callback text is too long");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MessageEditBotParams {
    #[serde(default)]
    pub chat_id: Option<i64>,
    #[serde(default)]
    pub message_id: Option<i64>,
    #[serde(default)]
    pub inline_message_id: Option<String>,
    pub text: String,
    #[serde(default)]
    pub buttons: Vec<Vec<InlineButton>>,
}

impl MessageEditBotParams {
    pub fn validate(&self, prefix_len: usize) -> Result<(), &'static str> {
        let inline = self
            .inline_message_id
            .as_deref()
            .is_some_and(|id| !id.is_empty());
        let chat = self.chat_id.is_some_and(|chat_id| chat_id != 0)
            && self.message_id.is_some_and(|message_id| message_id > 0);
        if !inline && !chat {
            return Err("inline_message_id or chat_id and message_id are required");
        }
        if self.text.is_empty() || self.text.contains('\0') {
            return Err("text is required");
        }
        if self.text.encode_utf16().count() > 4096 {
            return Err("text is too long");
        }
        validate_button_rows(&self.buttons, prefix_len)
    }
}

/// Offline boundary for companion-bot posts. The runtime integration supplies
/// the token-backed implementation; production and tests share this trait so
/// the host executor never touches HTTP or credentials directly.
pub trait HostBotSend: Send + Sync {
    fn send<'a>(
        &'a self,
        chat_id: i64,
        message_thread_id: Option<i32>,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// One inline-keyboard button of a companion-bot menu. `data` is module-
/// defined and is namespaced by the host before it reaches Bot API.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct InlineButton {
    pub text: String,
    pub data: String,
}

pub const MAX_INLINE_BUTTON_DATA_BYTES: usize = 32;
pub const MAX_INLINE_ROWS: usize = 16;
pub const MAX_INLINE_BUTTONS_PER_ROW: usize = 8;

impl InlineButton {
    pub fn validate(&self, prefix_len: usize) -> Result<(), &'static str> {
        if self.text.is_empty() || self.text.encode_utf16().count() > 64 {
            return Err("button text must be 1..=64 utf16 units");
        }
        if self.data.is_empty()
            || self.data.contains('\0')
            || self.data.len() + prefix_len + 1 > MAX_INLINE_BUTTON_DATA_BYTES * 2
        {
            return Err("button data is too long");
        }
        Ok(())
    }
}

pub fn validate_button_rows(
    rows: &[Vec<InlineButton>],
    prefix_len: usize,
) -> Result<(), &'static str> {
    if rows.len() > MAX_INLINE_ROWS {
        return Err("too many button rows");
    }
    for row in rows {
        if row.is_empty() || row.len() > MAX_INLINE_BUTTONS_PER_ROW {
            return Err("button row must hold 1..=8 buttons");
        }
        for button in row {
            button.validate(prefix_len)?;
        }
    }
    Ok(())
}

/// The companion-bot interactive surface used by `inline.form`,
/// `inline.answer`, and `message.editBot`. Implementations own the inline
/// menu registry, the Bot API boundary, and the user-session inline bridge.
pub trait HostInlineSurface: Send + Sync {
    /// Publish the prepared menu and send it into `peer` as a via-bot
    /// message. `data_prefix` is the `<module_id>|` callback namespace.
    fn form<'a>(
        &'a self,
        peer: grammers_session::types::PeerId,
        text: &'a str,
        buttons: &'a [Vec<InlineButton>],
        data_prefix: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
    /// Acknowledge a callback press (toast or alert).
    fn answer<'a>(
        &'a self,
        callback_id: &'a str,
        text: &'a str,
        show_alert: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
    /// Redraw a bot-sent inline form message in place. Either
    /// `inline_message_id` (via-bot messages) or `chat_id` + `message_id`
    /// identifies the target; the other one is empty/zero.
    fn edit<'a>(
        &'a self,
        chat_id: i64,
        message_id: i64,
        inline_message_id: &'a str,
        text: &'a str,
        buttons: &'a [Vec<InlineButton>],
        data_prefix: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// Narrow host-side validation boundary. Telegram mutation is deliberately
/// supplied by the runtime integration, while this type owns capability and
/// opaque-handle checks shared by production and tests.
pub struct V6HostExecutor {
    module_id: String,
    handles: Arc<Mutex<V6HandleRegistry>>,
    can_edit: bool,
    can_send_bot: bool,
    inline_ok: bool,
    ledger: SharedSelfEditLedger,
    bot: Option<Arc<dyn HostBotSend>>,
    inline: Option<Arc<dyn HostInlineSurface>>,
}

impl Default for V6HostExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl V6HostExecutor {
    pub fn new() -> Self {
        Self::from_capability(false)
    }
    pub fn from_capability(can_edit: bool) -> Self {
        Self::with_registry(
            String::new(),
            can_edit,
            Arc::new(Mutex::new(V6HandleRegistry::new())),
            SharedSelfEditLedger::default(),
        )
    }
    pub(crate) fn with_registry(
        module_id: String,
        can_edit: bool,
        handles: Arc<Mutex<V6HandleRegistry>>,
        ledger: SharedSelfEditLedger,
    ) -> Self {
        Self {
            module_id,
            handles,
            can_edit,
            can_send_bot: false,
            inline_ok: false,
            ledger,
            bot: None,
            inline: None,
        }
    }
    pub(crate) fn with_bot_send(
        mut self,
        can_send_bot: bool,
        bot: Option<Arc<dyn HostBotSend>>,
    ) -> Self {
        self.can_send_bot = can_send_bot;
        self.bot = bot;
        self
    }
    pub(crate) fn with_inline_surface(
        mut self,
        can_inline: bool,
        surface: Option<Arc<dyn HostInlineSurface>>,
    ) -> Self {
        self.inline_ok = can_inline;
        self.inline = surface;
        self
    }
    /// Callback-data namespace: `<module_id>|<data>` keeps presses routable
    /// to the owning module without trusting module-supplied routing.
    pub(crate) fn data_prefix(&self) -> String {
        format!("{}|", self.module_id)
    }
    pub fn validate_edit(&mut self, params: &MessageEditParams) -> Result<(), &'static str> {
        if !self.can_edit {
            return Err("capability denied");
        }
        params.validate()?;
        self.handles
            .lock()
            .map_err(|_| "invalid message handle")?
            .resolve(&params.message, V6HandleKind::Message)
            .map_err(|_| "invalid message handle")
    }
    pub(crate) async fn execute(
        &self,
        method: &str,
        params: Box<RawValue>,
    ) -> Result<serde_json::Value, &'static str> {
        match method {
            "message.edit" => self.execute_edit(params).await,
            "message.sendBot" => self.execute_send_bot(params).await,
            "inline.form" => self.execute_inline_form(params).await,
            "inline.answer" => self.execute_inline_answer(params).await,
            "message.editBot" => self.execute_message_edit_bot(params).await,
            _ => Err("unknown host method"),
        }
    }

    async fn execute_inline_form(
        &self,
        params: Box<RawValue>,
    ) -> Result<serde_json::Value, &'static str> {
        if !self.inline_ok {
            return Err("capability denied");
        }
        let surface = self.inline.as_ref().ok_or("inline surface unavailable")?;
        let prefix = self.data_prefix();
        let params: InlineFormParams =
            serde_json::from_str(params.get()).map_err(|_| "invalid params")?;
        params.validate(prefix.len())?;
        let peer = self
            .handles
            .lock()
            .map_err(|_| "invalid peer handle")?
            .resolve_peer(&params.peer)
            .map_err(|_| "invalid peer handle")?;
        surface
            .form(peer, &params.text, &params.buttons, &prefix)
            .await
            .map_err(|message| match message.as_str() {
                "inline form rejected" => "inline form rejected",
                "inline form timeout" => "inline form timeout",
                _ => "inline form unavailable",
            })?;
        Ok(serde_json::Value::Null)
    }

    async fn execute_inline_answer(
        &self,
        params: Box<RawValue>,
    ) -> Result<serde_json::Value, &'static str> {
        if !self.inline_ok {
            return Err("capability denied");
        }
        let surface = self.inline.as_ref().ok_or("inline surface unavailable")?;
        let params: InlineAnswerParams =
            serde_json::from_str(params.get()).map_err(|_| "invalid params")?;
        params.validate()?;
        surface
            .answer(&params.callback_id, &params.text, params.show_alert)
            .await
            .map_err(|message| match message.as_str() {
                "callback answer rejected" => "callback answer rejected",
                "callback answer timeout" => "callback answer timeout",
                _ => "callback answer unavailable",
            })?;
        Ok(serde_json::Value::Null)
    }

    async fn execute_message_edit_bot(
        &self,
        params: Box<RawValue>,
    ) -> Result<serde_json::Value, &'static str> {
        if !self.inline_ok {
            return Err("capability denied");
        }
        let surface = self.inline.as_ref().ok_or("inline surface unavailable")?;
        let prefix = self.data_prefix();
        let params: MessageEditBotParams =
            serde_json::from_str(params.get()).map_err(|_| "invalid params")?;
        params.validate(prefix.len())?;
        let inline_message_id = params
            .inline_message_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .unwrap_or("");
        // The edit arrives back as a self-authored MessageEdited update; arm
        // the ledger first so the runtime suppresses it before command
        // routing sees module-controlled text.
        let peer_id = peer_id_from_bot_chat_id(params.chat_id.unwrap_or(0));
        let message_id = params.message_id.unwrap_or(0);
        if let Some(peer_id) = peer_id {
            let _ = self
                .ledger
                .register(peer_id, message_id as i32, params.text.clone());
        }
        let result = surface
            .edit(
                params.chat_id.unwrap_or(0),
                message_id,
                inline_message_id,
                &params.text,
                &params.buttons,
                &prefix,
            )
            .await
            .map_err(|message| match message.as_str() {
                "bot edit rejected" => "bot edit rejected",
                "bot edit timeout" => "bot edit timeout",
                _ => "bot edit unavailable",
            });
        if result.is_err()
            && let Some(peer_id) = peer_id
        {
            self.ledger.remove(peer_id, message_id as i32, &params.text);
        }
        result.map(|_| serde_json::Value::Null)
    }

    async fn execute_edit(&self, params: Box<RawValue>) -> Result<serde_json::Value, &'static str> {
        if !self.can_edit {
            return Err("capability denied");
        }
        let params: MessageEditParams =
            serde_json::from_str(params.get()).map_err(|_| "invalid params")?;
        params.validate()?;
        let message = self
            .handles
            .lock()
            .map_err(|_| "invalid message handle")?
            .resolve_message(&params.message)
            .map_err(|_| "invalid message handle")?;
        if !message.outgoing() {
            return Err("message is not outgoing");
        }
        self.ledger
            .register(message.peer_id(), message.id(), params.text.clone())
            .map_err(|_| "suppression capacity exhausted")?;
        let input = grammers_client::message::InputMessage::new().text(params.text.clone());
        match message.edit(input).await {
            Ok(_) => Ok(serde_json::Value::Null),
            Err(error) if error.is("MESSAGE_NOT_MODIFIED") => {
                self.ledger
                    .remove(message.peer_id(), message.id(), &params.text);
                Ok(serde_json::Value::Null)
            }
            Err(error) if edit_definitely_rejected(&error) => {
                self.ledger
                    .remove(message.peer_id(), message.id(), &params.text);
                if error.is("MESSAGE_NOT_MODIFIED") {
                    Ok(serde_json::Value::Null)
                } else {
                    Err("edit rejected")
                }
            }
            Err(_) => Err("edit unavailable"),
        }
    }

    async fn execute_send_bot(
        &self,
        params: Box<RawValue>,
    ) -> Result<serde_json::Value, &'static str> {
        if !self.can_send_bot {
            return Err("capability denied");
        }
        let params: MessageSendBotParams =
            serde_json::from_str(params.get()).map_err(|_| "invalid params")?;
        params.validate()?;
        let bot = self.bot.as_ref().ok_or("bot sender unavailable")?;
        bot.send(params.chat_id, params.message_thread_id, &params.text)
            .await
            .map_err(|message| match message.as_str() {
                "bot send rejected" => "bot send rejected",
                "bot send timeout" => "bot send timeout",
                _ => "bot send unavailable",
            })?;
        Ok(serde_json::Value::Null)
    }
}

/// Bot API chat ids map onto MTProto peers by convention: users are
/// positive, basic groups are negative small ids, and channels/supergroups
/// carry the -100 prefix. Supergroup migration invalidates the mapping on
/// the Bot API side only, which the ledger treats as a plain miss.
fn peer_id_from_bot_chat_id(chat_id: i64) -> Option<PeerId> {
    if chat_id >= 0 {
        PeerId::user(chat_id)
    } else if chat_id <= -1_000_000_000_000 {
        PeerId::channel(-(chat_id + 1_000_000_000_000))
    } else {
        PeerId::chat(-chat_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_validation_enforces_strict_text_limits() {
        let base = |text: String| MessageEditParams {
            message: "handle".into(),
            text,
        };
        assert!(base("x".repeat(4096)).validate().is_ok());
        assert!(base("x".repeat(4097)).validate().is_err());
        assert!(base("😀".repeat(2048)).validate().is_ok());
        assert!(base("😀".repeat(2049)).validate().is_err());
        assert!(base("x\0y".into()).validate().is_err());
        assert!(
            serde_json::from_str::<MessageEditParams>(r#"{"message":"h","text":"x","extra":1}"#)
                .is_err()
        );
        assert!(
            V6HostExecutor::new()
                .validate_edit(&base("x".into()))
                .is_err()
        );
    }

    #[test]
    fn send_bot_validation_enforces_destination_and_text_limits() {
        let base = |chat_id: i64, thread: Option<i32>, text: String| MessageSendBotParams {
            chat_id,
            message_thread_id: thread,
            text,
        };
        assert!(base(-100123, Some(7), "x".into()).validate().is_ok());
        assert!(base(0, None, "x".into()).validate().is_err());
        assert!(base(-100123, Some(0), "x".into()).validate().is_err());
        assert!(base(-100123, Some(-5), "x".into()).validate().is_err());
        assert!(base(-100123, None, String::new()).validate().is_err());
        assert!(base(-100123, None, "x\0y".into()).validate().is_err());
        assert!(base(-100123, None, "x".repeat(4097)).validate().is_err());
        assert!(
            serde_json::from_str::<MessageSendBotParams>(r#"{"chat_id":1,"text":"x","extra":1}"#)
                .is_err()
        );
    }

    struct RecordingBot {
        allowed: bool,
    }

    impl HostBotSend for RecordingBot {
        fn send<'a>(
            &'a self,
            chat_id: i64,
            message_thread_id: Option<i32>,
            text: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async move {
                if !self.allowed {
                    return Err("bot send rejected".into());
                }
                assert_eq!(chat_id, -100123);
                assert_eq!(message_thread_id, Some(7));
                assert_eq!(text, "hello");
                Ok(())
            })
        }
    }

    fn raw_params(json: &str) -> Box<RawValue> {
        RawValue::from_string(json.to_owned()).unwrap()
    }

    #[tokio::test]
    async fn send_bot_is_capability_gated_and_routes_through_the_sender() {
        let bot: Arc<dyn HostBotSend> = Arc::new(RecordingBot { allowed: true });
        let executor = V6HostExecutor::new().with_bot_send(true, Some(bot));
        let value = executor
            .execute(
                "message.sendBot",
                raw_params(r#"{"chat_id":-100123,"message_thread_id":7,"text":"hello"}"#),
            )
            .await
            .unwrap();
        assert_eq!(value, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn send_bot_denies_missing_capability_and_unknown_methods() {
        let bot: Arc<dyn HostBotSend> = Arc::new(RecordingBot { allowed: true });
        let denied = V6HostExecutor::new().with_bot_send(false, Some(bot.clone()));
        assert_eq!(
            denied
                .execute(
                    "message.sendBot",
                    raw_params(r#"{"chat_id":-100123,"text":"hello"}"#)
                )
                .await,
            Err("capability denied")
        );
        assert_eq!(
            denied
                .execute("message.edit", raw_params(r#"{"message":"h","text":"x"}"#))
                .await,
            Err("capability denied")
        );
        assert_eq!(
            denied
                .execute("message.delete", raw_params(r#"{"chat_id":1}"#))
                .await,
            Err("unknown host method")
        );
    }

    #[tokio::test]
    async fn send_bot_maps_sender_failures_to_sanitized_messages() {
        let bot: Arc<dyn HostBotSend> = Arc::new(RecordingBot { allowed: false });
        let executor = V6HostExecutor::new().with_bot_send(true, Some(bot));
        assert_eq!(
            executor
                .execute(
                    "message.sendBot",
                    raw_params(r#"{"chat_id":-100123,"text":"hello"}"#)
                )
                .await,
            Err("bot send rejected")
        );
    }
}
