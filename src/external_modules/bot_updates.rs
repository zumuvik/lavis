//! Long-poll loop for the companion bot: stages `inline.form` menus into
//! inline-query answers and routes callback presses of the signed-in user to
//! the owning module.
//!
//! Request URLs contain the bot token, so transport errors are logged as
//! sanitized kind-only strings and never include the URL.

use super::bot_send::InlineMenuRegistry;
use super::protocol::BotCallbackEvent;
use crate::setup_store::{CompanionToken, SetupStore};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MAX_UPDATES_BODY_BYTES: usize = 256 * 1024;
const LONG_POLL_TIMEOUT: Duration = Duration::from_secs(35);
const LONG_POLL_SERVER_WAIT: i64 = 25;
const ERROR_BACKOFF: Duration = Duration::from_secs(5);
const MAX_ERROR_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF_STEPS: u32 = 12;

pub struct BotUpdatesConfig {
    pub state_path: PathBuf,
    pub token_path: PathBuf,
    pub registry: Arc<InlineMenuRegistry>,
    pub self_user_id: i64,
    pub manager: super::manager::ExternalManagerHandle,
}

pub fn spawn(config: BotUpdatesConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(config))
}

async fn run(config: BotUpdatesConfig) {
    let client = match reqwest::Client::builder().https_only(true).build() {
        Ok(client) => client,
        Err(_) => {
            tracing::warn!(
                event = "external_module_bot_updates_error",
                error = "client init failed",
                "Companion bot updates disabled"
            );
            return;
        }
    };
    let mut offset: i64 = 0;
    let mut failure_streak: u32 = 0;
    let mut swallow_first_batch = true;
    loop {
        let token = match load_token(&config.state_path, &config.token_path).await {
            Ok(token) => token,
            Err(_) => {
                tracing::warn!(
                    event = "external_module_bot_updates_error",
                    error = "companion bot token unavailable",
                    "Companion bot token is not readable yet"
                );
                tokio::time::sleep(ERROR_BACKOFF).await;
                continue;
            }
        };
        let payload = serde_json::json!({
            "offset": offset,
            "timeout": LONG_POLL_SERVER_WAIT,
            "allowed_updates": ["inline_query", "callback_query"],
        });
        let url = format!("https://api.telegram.org/bot{}/getUpdates", token.as_str());
        let response = match client
            .post(url)
            .timeout(LONG_POLL_TIMEOUT)
            .json(&payload)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                log_transport_error(&error);
                tokio::time::sleep(ERROR_BACKOFF).await;
                continue;
            }
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let kind = match status {
                409 => "getUpdates conflict (webhook or second consumer)",
                401 | 404 => "bot token rejected",
                _ => "getUpdates rejected",
            };
            failure_streak = failure_streak.saturating_add(1);
            tracing::warn!(
                event = "external_module_bot_updates_error",
                status,
                error = kind,
                "Companion bot getUpdates failed"
            );
            let backoff = ERROR_BACKOFF
                .saturating_mul(failure_streak.min(MAX_BACKOFF_STEPS))
                .min(MAX_ERROR_BACKOFF);
            tokio::time::sleep(backoff).await;
            continue;
        }
        failure_streak = 0;
        let body = match read_bounded_body(response).await {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(
                    event = "external_module_bot_updates_error",
                    error = error,
                    "Companion bot getUpdates body rejected"
                );
                tokio::time::sleep(ERROR_BACKOFF).await;
                continue;
            }
        };
        let batch = match parse_updates(&body) {
            Some(batch) => batch,
            None => {
                tracing::warn!(
                    event = "external_module_bot_updates_error",
                    error = "malformed getUpdates body",
                    "Companion bot getUpdates body rejected"
                );
                tokio::time::sleep(ERROR_BACKOFF).await;
                continue;
            }
        };
        if swallow_first_batch {
            // Updates queued before (re)start are stale: their callback ids
            // expire within seconds, so replaying them only produces noise.
            swallow_first_batch = false;
            if let Some(max_id) = batch.iter().map(|update| update.update_id).max() {
                offset = max_id + 1;
            }
            continue;
        }
        if let Some(max_id) = batch.iter().map(|update| update.update_id).max() {
            offset = max_id + 1;
        }
        for update in batch {
            handle_update(&config, &client, &token, update).await;
        }
    }
}

async fn handle_update(
    config: &BotUpdatesConfig,
    client: &reqwest::Client,
    token: &CompanionToken,
    update: Update,
) {
    if let Some(query) = update.inline_query {
        answer_inline_query(config, client, token, query).await;
    }
    if let Some(query) = update.callback_query {
        handle_callback_query(config, client, token, query).await;
    }
}

async fn answer_inline_query(
    config: &BotUpdatesConfig,
    client: &reqwest::Client,
    token: &CompanionToken,
    query: InlineQuery,
) {
    let mut payload = serde_json::json!({ "inline_query_id": query.id, "cache_time": 0 });
    if let Some(menu) = config.registry.take(&query.query) {
        let mut id_bytes = [0u8; 8];
        let result_id: String = match getrandom::fill(&mut id_bytes) {
            Ok(()) => id_bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
            Err(_) => {
                tracing::warn!(
                    event = "external_module_bot_updates_error",
                    error = "entropy unavailable",
                    "Could not build inline answer id"
                );
                return;
            }
        };
        let keyboard: Vec<Vec<serde_json::Value>> = menu
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(text, data)| serde_json::json!({ "text": text, "callback_data": data }))
                    .collect()
            })
            .collect();
        payload["results"] = serde_json::json!([{
            "type": "article",
            "id": result_id,
            "title": "Lavis",
            "input_message_content": { "message_text": menu.text },
            "reply_markup": { "inline_keyboard": keyboard },
        }]);
    } else {
        payload["results"] = serde_json::json!([]);
    }
    post_bot_request(client, token, "answerInlineQuery", &payload).await;
}

async fn handle_callback_query(
    config: &BotUpdatesConfig,
    client: &reqwest::Client,
    token: &CompanionToken,
    query: CallbackQuery,
) {
    if query.from.id != config.self_user_id {
        let payload = serde_json::json!({
            "callback_query_id": query.id,
            "text": "Недоступно",
        });
        post_bot_request(client, token, "answerCallbackQuery", &payload).await;
        return;
    }
    let Some(data) = query.data else {
        return;
    };
    let Some((module_id, inner)) = data.split_once('|') else {
        return;
    };
    let event = BotCallbackEvent {
        callback_id: query.id,
        data: inner.to_owned(),
        chat_id: query.message.as_ref().map_or(0, |m| m.chat.id),
        message_id: query.message.as_ref().map_or(0, |m| m.message_id),
        from_user_id: query.from.id,
    };
    let manager = config.manager.clone();
    let module_id = module_id.to_owned();
    tokio::spawn(async move {
        if let Err(error) = manager.bot_callback(&module_id, event).await {
            tracing::warn!(
                event = "external_module_bot_callback_failed",
                module_id = %module_id,
                error = %error,
                "Companion bot callback could not be delivered"
            );
        }
    });
}

async fn post_bot_request(
    client: &reqwest::Client,
    token: &CompanionToken,
    method: &str,
    payload: &serde_json::Value,
) {
    let url = format!("https://api.telegram.org/bot{}/{}", token.as_str(), method);
    let result = client
        .post(url)
        .timeout(LONG_POLL_TIMEOUT)
        .json(payload)
        .send()
        .await;
    if let Err(error) = result {
        log_transport_error(&error);
    }
}

fn log_transport_error(error: &reqwest::Error) {
    let kind = if error.is_timeout() {
        "timeout"
    } else {
        "transport"
    };
    tracing::warn!(
        event = "external_module_bot_updates_error",
        error = kind,
        "Companion bot request failed"
    );
}

async fn load_token(state_path: &Path, token_path: &Path) -> Result<CompanionToken, ()> {
    let state_path = state_path.to_path_buf();
    let token_path = token_path.to_path_buf();
    tokio::task::spawn_blocking(move || SetupStore::new(state_path, token_path).load_token())
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

async fn read_bounded_body(response: reqwest::Response) -> Result<Vec<u8>, &'static str> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_UPDATES_BODY_BYTES as u64)
    {
        return Err("oversized getUpdates body");
    }
    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "getUpdates read failed")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_UPDATES_BODY_BYTES {
            return Err("oversized getUpdates body");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Deserialize)]
struct UpdatesResponse {
    ok: bool,
    #[serde(default)]
    result: Vec<Update>,
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    inline_query: Option<InlineQuery>,
    #[serde(default)]
    callback_query: Option<CallbackQuery>,
}

#[derive(Deserialize)]
struct InlineQuery {
    id: String,
    #[serde(default)]
    query: String,
}

#[derive(Deserialize)]
struct CallbackQuery {
    id: String,
    from: FromUser,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    message: Option<Msg>,
}

#[derive(Deserialize)]
struct FromUser {
    id: i64,
}

#[derive(Deserialize)]
struct Msg {
    chat: Chat,
    message_id: i64,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
}

fn parse_updates(body: &[u8]) -> Option<Vec<Update>> {
    let parsed: UpdatesResponse = serde_json::from_slice(body).ok()?;
    parsed.ok.then_some(parsed.result)
}
