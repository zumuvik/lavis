//! Minimal, token-redacting Telegram Bot API validation client.

use std::{future::Future, pin::Pin, time::Duration};

use serde::Deserialize;

use crate::setup_store::CompanionToken;

pub type BotApiFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BotIdentity, BotApiError>> + Send + 'a>>;

/// The deliberately narrow HTTP boundary used by setup.  Tests can inject this
/// without opening a socket or ever constructing a token URL.
pub trait BotApi: Send + Sync {
    fn get_me<'a>(&'a self, token: &'a CompanionToken) -> BotApiFuture<'a>;
}

/// A plain-text message posted by the companion bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotMessage {
    pub chat_id: i64,
    pub message_thread_id: Option<i32>,
    pub text: String,
}

/// An in-place edit of a companion-bot inline form. Callback data is already
/// host-namespaced before it reaches this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotEditMessage {
    pub chat_id: i64,
    pub message_id: i64,
    pub text: String,
    pub buttons: Vec<Vec<(String, String)>>,
}

pub type BotSendFuture<'a> = Pin<Box<dyn Future<Output = Result<(), BotApiError>> + Send + 'a>>;

/// The deliberately narrow HTTP boundary used to post messages as the
/// companion bot. Separate from [`BotApi`] so setup validation cannot grow
/// send authority by accident.
pub trait BotSendApi: Send + Sync {
    fn send_message<'a>(
        &'a self,
        token: &'a CompanionToken,
        message: &'a BotMessage,
    ) -> BotSendFuture<'a>;
    fn answer_callback<'a>(
        &'a self,
        token: &'a CompanionToken,
        callback_id: &'a str,
        text: &'a str,
        show_alert: bool,
    ) -> BotSendFuture<'a>;
    fn edit_message_text<'a>(
        &'a self,
        token: &'a CompanionToken,
        message: &'a BotEditMessage,
    ) -> BotSendFuture<'a>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BotIdentity {
    pub id: i64,
    pub username: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BotApiError {
    Transport,
    Timeout,
    Rejected,
    Oversized,
    Malformed,
    NotBot,
    WrongUsername,
}

const MAX_GET_ME_BODY_BYTES: usize = 64 * 1024;

/// Uses Rustls only (via reqwest's `rustls-tls` feature).  Do not include the
/// request URL in errors: its path contains the token.
pub struct HttpBotApi {
    client: reqwest::Client,
}

impl HttpBotApi {
    pub fn new() -> Result<Self, BotApiError> {
        reqwest::Client::builder()
            .https_only(true)
            .timeout(Duration::from_secs(20))
            .build()
            .map(|client| Self { client })
            .map_err(|_| BotApiError::Transport)
    }
}

#[derive(Deserialize)]
struct GetMeResponse {
    ok: bool,
    result: Option<GetMeResult>,
}

#[derive(Deserialize)]
struct GetMeResult {
    id: i64,
    username: Option<String>,
    is_bot: bool,
}

fn parse_get_me_body(body: &[u8]) -> Result<BotIdentity, BotApiError> {
    let body: GetMeResponse = serde_json::from_slice(body).map_err(|_| BotApiError::Malformed)?;
    let Some(result) = body.ok.then_some(body.result).flatten() else {
        return Err(BotApiError::Rejected);
    };
    if !result.is_bot {
        return Err(BotApiError::NotBot);
    }
    let Some(username) = result.username.filter(|name| !name.is_empty()) else {
        return Err(BotApiError::WrongUsername);
    };
    if crate::setup::validate_username(&username).is_err() {
        return Err(BotApiError::WrongUsername);
    }
    Ok(BotIdentity {
        id: result.id,
        username,
    })
}

async fn read_bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, BotApiError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_GET_ME_BODY_BYTES as u64)
    {
        return Err(BotApiError::Oversized);
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        append_body_chunk(&mut body, &chunk)?;
    }
    Ok(body)
}

fn append_body_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), BotApiError> {
    if body.len().saturating_add(chunk.len()) > MAX_GET_ME_BODY_BYTES {
        return Err(BotApiError::Oversized);
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn request_error(error: reqwest::Error) -> BotApiError {
    if error.is_timeout() {
        BotApiError::Timeout
    } else {
        BotApiError::Transport
    }
}

impl BotApi for HttpBotApi {
    fn get_me<'a>(&'a self, token: &'a CompanionToken) -> BotApiFuture<'a> {
        Box::pin(async move {
            let url = format!("https://api.telegram.org/bot{}/getMe", token.as_str());
            let response = self.client.get(url).send().await.map_err(request_error)?;
            if !response.status().is_success() {
                return Err(BotApiError::Rejected);
            }
            let body = read_bounded_body(response).await?;
            parse_get_me_body(&body)
        })
    }
}

#[derive(Deserialize)]
struct BotSendResponse {
    ok: bool,
}

fn parse_send_body(body: &[u8]) -> Result<(), BotApiError> {
    let parsed: BotSendResponse =
        serde_json::from_slice(body).map_err(|_| BotApiError::Malformed)?;
    if parsed.ok {
        Ok(())
    } else {
        Err(BotApiError::Rejected)
    }
}

impl BotSendApi for HttpBotApi {
    fn send_message<'a>(
        &'a self,
        token: &'a CompanionToken,
        message: &'a BotMessage,
    ) -> BotSendFuture<'a> {
        Box::pin(async move {
            let url = format!("https://api.telegram.org/bot{}/sendMessage", token.as_str());
            let mut payload = serde_json::json!({
                "chat_id": message.chat_id,
                "text": message.text,
            });
            if let Some(thread) = message.message_thread_id {
                payload["message_thread_id"] = serde_json::json!(thread);
            }
            let response = self
                .client
                .post(url)
                .json(&payload)
                .send()
                .await
                .map_err(request_error)?;
            if !response.status().is_success() {
                return Err(BotApiError::Rejected);
            }
            let body = read_bounded_body(response).await?;
            parse_send_body(&body)
        })
    }

    fn answer_callback<'a>(
        &'a self,
        token: &'a CompanionToken,
        callback_id: &'a str,
        text: &'a str,
        show_alert: bool,
    ) -> BotSendFuture<'a> {
        Box::pin(async move {
            let url = format!(
                "https://api.telegram.org/bot{}/answerCallbackQuery",
                token.as_str()
            );
            let mut payload = serde_json::json!({ "callback_query_id": callback_id });
            if !text.is_empty() {
                payload["text"] = serde_json::json!(text);
            }
            if show_alert {
                payload["show_alert"] = serde_json::json!(true);
            }
            let response = self
                .client
                .post(url)
                .json(&payload)
                .send()
                .await
                .map_err(request_error)?;
            if !response.status().is_success() {
                return Err(BotApiError::Rejected);
            }
            let body = read_bounded_body(response).await?;
            parse_send_body(&body)
        })
    }

    fn edit_message_text<'a>(
        &'a self,
        token: &'a CompanionToken,
        message: &'a BotEditMessage,
    ) -> BotSendFuture<'a> {
        Box::pin(async move {
            let url = format!(
                "https://api.telegram.org/bot{}/editMessageText",
                token.as_str()
            );
            let mut payload = serde_json::json!({
                "chat_id": message.chat_id,
                "message_id": message.message_id,
                "text": message.text,
            });
            if !message.buttons.is_empty() {
                let keyboard: Vec<Vec<serde_json::Value>> = message
                    .buttons
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|(text, data)| {
                                serde_json::json!({ "text": text, "callback_data": data })
                            })
                            .collect()
                    })
                    .collect();
                payload["reply_markup"] = serde_json::json!({ "inline_keyboard": keyboard });
            }
            let response = self
                .client
                .post(url)
                .json(&payload)
                .send()
                .await
                .map_err(request_error)?;
            if !response.status().is_success() {
                return Err(BotApiError::Rejected);
            }
            let body = read_bounded_body(response).await?;
            parse_send_body(&body)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock;
    impl BotApi for Mock {
        fn get_me<'a>(&'a self, _: &'a CompanionToken) -> BotApiFuture<'a> {
            Box::pin(async {
                Ok(BotIdentity {
                    id: 7,
                    username: "lavis_test_bot".into(),
                })
            })
        }
    }

    #[tokio::test]
    async fn typed_mock_validates_without_network() {
        let token = CompanionToken::new("123456:abcdefghijklmnopqrstUVWX".into()).unwrap();
        assert_eq!(
            Mock.get_me(&token).await.unwrap().username,
            "lavis_test_bot"
        );
        assert!(!format!("{token:?}").contains(token.as_str()));
    }

    #[test]
    fn get_me_response_categories_are_deterministic() {
        assert_eq!(
            parse_get_me_body(
                br#"{"ok":true,"result":{"id":7,"username":"lavis_test_bot","is_bot":true}}"#
            )
            .unwrap(),
            BotIdentity {
                id: 7,
                username: "lavis_test_bot".into(),
            }
        );
        assert_eq!(
            parse_get_me_body(
                br#"{"ok":true,"result":{"id":7,"username":"lavis_test_bot","is_bot":false}}"#
            ),
            Err(BotApiError::NotBot)
        );
        assert_eq!(parse_get_me_body(b"not json"), Err(BotApiError::Malformed));
        assert_eq!(
            parse_get_me_body(
                br#"{"ok":true,"result":{"id":7,"username":"lavis_helper","is_bot":true}}"#
            ),
            Err(BotApiError::WrongUsername)
        );
    }

    #[test]
    fn oversized_body_is_rejected_before_json_parsing() {
        let mut body = Vec::new();
        assert_eq!(
            append_body_chunk(&mut body, &vec![b'x'; MAX_GET_ME_BODY_BYTES + 1]),
            Err(BotApiError::Oversized)
        );
        assert!(body.is_empty());
    }
}
