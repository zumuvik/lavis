//! Companion-bot post boundary for host.invoke `message.sendBot`.
//!
//! The module never sees bot credentials: it names a destination and text,
//! and this sender loads the companion token from the local setup store and
//! posts through the Bot API. Error strings are static so no token material
//! or HTTP detail can leak into module-visible diagnostics.

use super::v6_host::HostBotSend;
use crate::bot_api::{BotMessage, BotSendApi, HttpBotApi};
use crate::setup_store::SetupStore;
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

pub struct CompanionBotSender {
    state_path: PathBuf,
    token_path: PathBuf,
    api: HttpBotApi,
}

impl CompanionBotSender {
    pub fn new(
        state_path: PathBuf,
        token_path: PathBuf,
    ) -> Result<Self, crate::bot_api::BotApiError> {
        Ok(Self {
            state_path,
            token_path,
            api: HttpBotApi::new()?,
        })
    }

    fn sanitized_error(error: crate::bot_api::BotApiError) -> String {
        match error {
            crate::bot_api::BotApiError::Rejected => "bot send rejected".to_owned(),
            crate::bot_api::BotApiError::Timeout => "bot send timeout".to_owned(),
            _ => "bot send unavailable".to_owned(),
        }
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
            let token = {
                let state_path = self.state_path.clone();
                let token_path = self.token_path.clone();
                tokio::task::spawn_blocking(move || {
                    SetupStore::new(state_path, token_path).load_token()
                })
                .await
                .map_err(|_| "bot send unavailable".to_owned())?
                .map_err(|_| "companion bot is not configured".to_owned())?
            };
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

pub fn arc_sender(sender: CompanionBotSender) -> Arc<dyn HostBotSend> {
    Arc::new(sender)
}
