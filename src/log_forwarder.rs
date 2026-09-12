//! Forwarding of host-level WARN/ERROR tracing events to the companion
//! group's "Logs" forum topic via the companion bot.
//!
//! The tracing layer never logs, blocks, or panics: it only formats and
//! `try_send`s into a bounded channel; a worker does the network I/O and
//! swallows its own failures silently to avoid a feedback loop.

use crate::bot_api::{BotMessage, BotSendApi, HttpBotApi};
use crate::setup_store::SetupStore;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::mpsc::{self, Receiver, Sender, error::TrySendError};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::{Layer, registry::LookupSpan};

const MAX_LINE_CHARS: usize = 1000;
const MAX_EXTRA_FIELDS: usize = 3;
const SEND_BACKOFF: Duration = Duration::from_secs(60);
const DESTINATION_RETRY: Duration = Duration::from_secs(60);

pub struct LogBridge {
    tx: Sender<String>,
    rx: Mutex<Option<Receiver<String>>>,
    dropped: AtomicU64,
}

static BRIDGE: OnceLock<LogBridge> = OnceLock::new();

/// Installs the process-global bridge. The first installation wins; later
/// calls keep the existing bridge.
pub fn install_bridge(capacity: usize) {
    let (tx, rx) = mpsc::channel(capacity);
    let _ = BRIDGE.set(LogBridge {
        tx,
        rx: Mutex::new(Some(rx)),
        dropped: AtomicU64::new(0),
    });
}

/// The tracing layer forwarding WARN/ERROR events into the installed bridge.
pub fn bridge_layer<S>() -> impl Layer<S> + Send + Sync
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    BridgeLayer
}

struct BridgeLayer;

struct LineVisitor {
    message: Option<String>,
    extras: Vec<(String, String)>,
}

impl Visit for LineVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" && self.message.is_none() {
            self.message = Some(rendered);
        } else if self.extras.len() < MAX_EXTRA_FIELDS {
            self.extras.push((field.name().to_owned(), rendered));
        }
    }
}

impl<S: Subscriber> Layer<S> for BridgeLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if *metadata.level() < tracing::Level::WARN {
            return;
        }
        if metadata.target().starts_with("lavis_log_forwarder") {
            return;
        }
        let Some(bridge) = BRIDGE.get() else {
            return;
        };
        let mut visitor = LineVisitor {
            message: None,
            extras: Vec::new(),
        };
        event.record(&mut visitor);
        let Some(message) = visitor.message else {
            return;
        };
        let mut text = message;
        for (key, value) in visitor.extras {
            text.push_str(&format!(" {key}={value}"));
        }
        let line = format_line(metadata.level().as_str(), metadata.target(), &text);
        if let Err(TrySendError::Full(_) | TrySendError::Closed(_)) = bridge.tx.try_send(line) {
            count_dropped(bridge);
        }
    }
}

fn count_dropped(bridge: &LogBridge) {
    bridge.dropped.fetch_add(1, Ordering::Relaxed);
}

fn format_line(level: &str, target: &str, message: &str) -> String {
    let emoji = if level == "WARN" { "⚠️" } else { "❌" };
    let mut line = format!("{emoji} {target}: {message}");
    if line.chars().count() > MAX_LINE_CHARS {
        line = line.chars().take(MAX_LINE_CHARS).collect();
    }
    line
}

/// Starts the worker draining the installed bridge. Without an installed
/// bridge this is a no-op.
pub fn spawn_worker(state_path: PathBuf, token_path: PathBuf) {
    let Some(bridge) = BRIDGE.get() else {
        return;
    };
    let Some(rx) = bridge
        .rx
        .lock()
        .map(|mut guard| guard.take())
        .unwrap_or(None)
    else {
        return;
    };
    tokio::spawn(worker(state_path, token_path, rx));
}

async fn worker(state_path: PathBuf, token_path: PathBuf, mut rx: Receiver<String>) {
    let api = match HttpBotApi::new() {
        Ok(api) => api,
        Err(_) => return,
    };
    let client = reqwest::Client::builder().https_only(true).build().ok();
    let mut token: Option<crate::setup_store::CompanionToken> = None;
    let mut chat_id: Option<i64> = None;
    let mut topic_id: Option<i32> = None;
    let mut topic_unavailable = false;
    let mut topic_creation_attempted = false;
    let mut send_rejected_warned = false;

    while let Some(line) = rx.recv().await {
        if token.is_none() {
            token = load_token(&state_path, &token_path).await;
        }
        let Some(loaded_token) = token.clone() else {
            continue;
        };
        if chat_id.is_none() {
            chat_id = load_companion_chat_id(&state_path, &token_path).await;
        }
        let Some(resolved_chat_id) = chat_id else {
            // Setup may complete later; drain slowly until it does.
            tokio::time::sleep(DESTINATION_RETRY).await;
            continue;
        };
        if topic_id.is_none() && !topic_unavailable {
            topic_id = load_logs_topic_id(&state_path, &token_path).await;
            if topic_id.is_none() && !topic_creation_attempted {
                topic_creation_attempted = true;
                topic_id = match client.as_ref() {
                    Some(client) => {
                        create_logs_topic(
                            client,
                            &loaded_token,
                            resolved_chat_id,
                            &state_path,
                            &token_path,
                        )
                        .await
                    }
                    None => None,
                };
                if topic_id.is_none() {
                    topic_unavailable = true;
                    // The bridge layer excludes this target from re-forwarding,
                    // so this warn reaches journalctl without a feedback loop.
                    tracing::warn!(
                        target: "lavis_log_forwarder",
                        event = "log_forwarder_topic_unavailable",
                        "Logs topic is unavailable — log lines are dropped until restart; run ,setup repair"
                    );
                }
            }
        }
        let Some(resolved_topic_id) = topic_id else {
            // Topic creation failed for this process: lines already reached
            // tracing, dropping them is the only remaining option.
            continue;
        };

        let dropped = BRIDGE
            .get()
            .map(|bridge| bridge.dropped.swap(0, Ordering::Relaxed))
            .unwrap_or(0);
        let text = if dropped > 0 {
            format!("… (и ещё {dropped} строк потеряно)\n{line}")
        } else {
            line
        };
        let message = BotMessage {
            chat_id: resolved_chat_id,
            message_thread_id: Some(resolved_topic_id),
            text,
        };
        if let Err(error) = api.send_message(&loaded_token, &message).await {
            if matches!(error, crate::bot_api::BotApiError::Rejected) {
                // The token may have been rotated or revoked: reload lazily.
                token = None;
            }
            if !send_rejected_warned {
                send_rejected_warned = true;
                tracing::warn!(
                    target: "lavis_log_forwarder",
                    event = "log_forwarder_send_rejected",
                    "Companion bot rejected log delivery — dropping lines"
                );
            }
            // Never tracing::log here: the layer would re-forward this
            // module's own events (feedback loop). Just back off.
            tokio::time::sleep(SEND_BACKOFF).await;
        }
    }
}

async fn load_token(
    state_path: &Path,
    token_path: &Path,
) -> Option<crate::setup_store::CompanionToken> {
    let state_path = state_path.to_path_buf();
    let token_path = token_path.to_path_buf();
    tokio::task::spawn_blocking(move || SetupStore::new(state_path, token_path).load_token())
        .await
        .ok()?
        .ok()
}

async fn load_state(
    state_path: PathBuf,
    token_path: PathBuf,
) -> Option<crate::setup_store::PersistedSetupState> {
    tokio::task::spawn_blocking(move || SetupStore::new(state_path, token_path).load_state())
        .await
        .ok()?
        .ok()
}

async fn load_companion_chat_id(state_path: &Path, token_path: &Path) -> Option<i64> {
    let state = load_state(state_path.to_path_buf(), token_path.to_path_buf()).await?;
    state.identities.companion_chat_id
}

async fn load_logs_topic_id(state_path: &Path, token_path: &Path) -> Option<i32> {
    let state = load_state(state_path.to_path_buf(), token_path.to_path_buf()).await?;
    state.identities.companion_logs_topic_id
}

#[derive(Deserialize)]
struct ForumTopicResponse {
    ok: bool,
    result: Option<ForumTopicResult>,
}

#[derive(Deserialize)]
struct ForumTopicResult {
    message_thread_id: i32,
}

/// Creates the "Logs" forum topic once per process and persists its id in the
/// setup state so later restarts reuse it.
async fn create_logs_topic(
    client: &reqwest::Client,
    token: &crate::setup_store::CompanionToken,
    chat_id: i64,
    state_path: &Path,
    token_path: &Path,
) -> Option<i32> {
    let url = format!(
        "https://api.telegram.org/bot{}/createForumTopic",
        token.as_str()
    );
    let response = client
        .post(url)
        .timeout(SEND_BACKOFF)
        .json(&serde_json::json!({ "chat_id": chat_id, "name": "Logs" }))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.bytes().await.ok()?;
    if body.len() > 64 * 1024 {
        return None;
    }
    let parsed: ForumTopicResponse = serde_json::from_slice(&body).ok()?;
    let thread_id = parsed
        .ok
        .then_some(parsed.result)
        .flatten()?
        .message_thread_id;
    let state_path = state_path.to_path_buf();
    let token_path = token_path.to_path_buf();
    // Persisting the topic id is best-effort: the process can still use it.
    let _ = tokio::task::spawn_blocking(move || {
        let mut store = SetupStore::new(state_path, token_path);
        let mut state = store.load_state()?;
        state.identities.companion_logs_topic_id = Some(thread_id);
        store.save_state(&state)
    })
    .await;
    Some(thread_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_line_uses_level_emoji_and_target() {
        assert_eq!(format_line("WARN", "lavis::x", "boom"), "⚠️ lavis::x: boom");
        assert_eq!(
            format_line("ERROR", "lavis::y", "dead"),
            "❌ lavis::y: dead"
        );
        assert_eq!(format_line("INFO", "lavis::z", "no"), "❌ lavis::z: no");
    }

    #[test]
    fn format_line_truncates_at_1000_chars() {
        let line = format_line("WARN", "t", &"x".repeat(5000));
        assert_eq!(line.chars().count(), MAX_LINE_CHARS);
        assert!(line.starts_with("⚠️ t: "));
    }

    #[tokio::test]
    async fn bridge_channel_bounds_and_drop_counter() {
        install_bridge(1);
        let bridge = BRIDGE.get().unwrap();
        bridge.tx.try_send("a".to_owned()).expect("first line fits");
        // Second line cannot fit (nobody receives yet): the layer must count it.
        match bridge.tx.try_send("b".to_owned()) {
            Err(TrySendError::Full(_) | TrySendError::Closed(_)) => count_dropped(bridge),
            Ok(()) => {}
        }
        assert_eq!(bridge.dropped.load(Ordering::Relaxed), 1);
        let rx = bridge.rx.lock().unwrap().take().unwrap();
        drop(rx);
        // Closed channel also counts as dropped.
        match bridge.tx.try_send("c".to_owned()) {
            Err(TrySendError::Full(_) | TrySendError::Closed(_)) => count_dropped(bridge),
            Ok(()) => {}
        }
        assert_eq!(bridge.dropped.load(Ordering::Relaxed), 2);
        // The worker drains the accounting when it finally sends a line.
        assert_eq!(bridge.dropped.swap(0, Ordering::Relaxed), 2);
        assert_eq!(bridge.dropped.load(Ordering::Relaxed), 0);
    }
}
