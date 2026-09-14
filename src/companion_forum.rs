//! Shared companion-forum topic helpers. Both the log forwarder ("Logs") and
//! the reactions audit ("Reactions") create a Bot API forum topic on first
//! use and persist its id in the setup state so restarts reuse it.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::setup_store::{CompanionToken, SetupStore};

const CREATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The companion forum topics Lavis manages. The mapping to the persisted
/// identity field lives here so callers cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompanionTopic {
    Logs,
    Reactions,
}

impl CompanionTopic {
    pub(crate) fn name(self) -> &'static str {
        match self {
            CompanionTopic::Logs => "Logs",
            CompanionTopic::Reactions => "Reactions",
        }
    }

    fn persisted_id(self, state: &crate::setup_store::PersistedSetupState) -> Option<i32> {
        match self {
            CompanionTopic::Logs => state.identities.companion_logs_topic_id,
            CompanionTopic::Reactions => state.identities.companion_reactions_topic_id,
        }
    }

    fn set_persisted_id(self, state: &mut crate::setup_store::PersistedSetupState, id: i32) {
        match self {
            CompanionTopic::Logs => state.identities.companion_logs_topic_id = Some(id),
            CompanionTopic::Reactions => state.identities.companion_reactions_topic_id = Some(id),
        }
    }
}

pub(crate) async fn load_persisted_topic_id(
    state_path: &Path,
    token_path: &Path,
    topic: CompanionTopic,
) -> Option<i32> {
    let state = load_state(state_path.to_path_buf(), token_path.to_path_buf()).await?;
    topic.persisted_id(&state)
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

#[derive(Deserialize)]
struct ForumTopicResponse {
    ok: bool,
    result: Option<ForumTopicResult>,
    description: Option<String>,
}

/// Forum topic creation is best-effort; the target is excluded from log
/// forwarding, so these warnings cannot loop back into the companion bot.
fn warn_topic_create(topic: &CompanionTopic, reason: &str, detail: Option<&str>) {
    tracing::warn!(
        target: "lavis_log_forwarder",
        event = "forum_topic_create_failed",
        topic = topic.name(),
        reason,
        detail = detail.unwrap_or(""),
        "Could not create companion forum topic"
    );
}

#[derive(Deserialize)]
struct ForumTopicResult {
    message_thread_id: i32,
}

/// Returns the persisted topic id, creating the topic on first use. Creation
/// and persistence are both best-effort: `None` means "no topic this time".
pub(crate) async fn ensure_forum_topic(
    client: &reqwest::Client,
    token: &CompanionToken,
    chat_id: i64,
    state_path: PathBuf,
    token_path: PathBuf,
    topic: CompanionTopic,
) -> Option<i32> {
    if let Some(id) = load_persisted_topic_id(&state_path, &token_path, topic).await {
        return Some(id);
    }
    let url = format!(
        "https://api.telegram.org/bot{}/createForumTopic",
        token.as_str()
    );
    let response = match client
        .post(url)
        .timeout(CREATE_TIMEOUT)
        .json(&serde_json::json!({ "chat_id": chat_id, "name": topic.name() }))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            warn_topic_create(&topic, "transport", Some(&error.to_string()));
            return None;
        }
    };
    if !response.status().is_success() {
        warn_topic_create(&topic, "http_status", Some(&response.status().to_string()));
        return None;
    }
    let body = response.bytes().await.ok()?;
    if body.len() > 64 * 1024 {
        warn_topic_create(&topic, "oversized_body", None);
        return None;
    }
    let parsed: ForumTopicResponse = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(error) => {
            warn_topic_create(&topic, "malformed", Some(&error.to_string()));
            return None;
        }
    };
    if !parsed.ok {
        warn_topic_create(&topic, "api_rejected", parsed.description.as_deref());
        return None;
    }
    let Some(result) = parsed.result else {
        warn_topic_create(&topic, "no_result", None);
        return None;
    };
    let thread_id = result.message_thread_id;
    // Persisting the topic id is best-effort: the process can still use it.
    let _ = tokio::task::spawn_blocking(move || {
        let mut store = SetupStore::new(state_path, token_path);
        let mut state = store.load_state()?;
        topic.set_persisted_id(&mut state, thread_id);
        store.save_state(&state)
    })
    .await;
    Some(thread_id)
}
