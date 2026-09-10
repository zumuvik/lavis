use super::{
    manifest::{ExternalCapability, ExternalModuleDescriptor},
    v6_handles::{V6HandleKind, V6HandleRegistry},
};
use crate::message_provenance::SharedSelfEditLedger;
use crate::message_provenance::edit_definitely_rejected;
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

/// Narrow host-side validation boundary. Telegram mutation is deliberately
/// supplied by the runtime integration, while this type owns capability and
/// opaque-handle checks shared by production and tests.
pub struct V6HostExecutor {
    handles: Arc<Mutex<V6HandleRegistry>>,
    can_edit: bool,
    can_send_bot: bool,
    ledger: SharedSelfEditLedger,
    bot: Option<Arc<dyn HostBotSend>>,
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
    pub fn from_descriptor(descriptor: &ExternalModuleDescriptor) -> Self {
        Self::from_capability(
            descriptor
                .capabilities
                .contains(&ExternalCapability::MessageEdit),
        )
    }
    fn from_capability(can_edit: bool) -> Self {
        Self::with_registry(
            can_edit,
            Arc::new(Mutex::new(V6HandleRegistry::new())),
            SharedSelfEditLedger::default(),
        )
    }
    pub(crate) fn with_registry(
        can_edit: bool,
        handles: Arc<Mutex<V6HandleRegistry>>,
        ledger: SharedSelfEditLedger,
    ) -> Self {
        Self {
            handles,
            can_edit,
            can_send_bot: false,
            ledger,
            bot: None,
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
            _ => Err("unknown host method"),
        }
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
