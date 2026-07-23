use anyhow::Context;
use grammers_client::{client::UpdateStream, update::Update};
use grammers_session::types::PeerId;

use crate::{
    command::parse,
    commands::{Action, dispatch},
    runtime::{RuntimeState, invocation_error_category},
};

pub async fn run(
    stream: &mut UpdateStream,
    self_user_id: PeerId,
    client: &grammers_client::Client,
    runtime: &mut RuntimeState,
) -> anyhow::Result<()> {
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            signal = &mut shutdown => {
                signal.context("failed to listen for Ctrl-C shutdown signal")?;
                stream
                    .sync_update_state()
                    .await
                    .map_err(anyhow::Error::from_boxed)
                    .context("failed to synchronize Telegram update state")?;
                return Ok(());
            }
            update = stream.next() => {
                let update = update.context("Telegram update stream ended or failed")?;
                process_update(update, self_user_id, client, runtime).await;
            }
        }
    }
}

async fn process_update(
    update: Update,
    self_user_id: PeerId,
    client: &grammers_client::Client,
    runtime: &mut RuntimeState,
) {
    let (message, edited) = match update {
        Update::NewMessage(message) => (message, false),
        Update::MessageEdited(message) => (message, true),
        _ => return,
    };
    let message_id = message.id();
    let peer_id = message.peer_id();
    if edited && runtime.consume_expected_self_edit(peer_id, message_id, message.text()) {
        tracing::debug!(
            event = "command_self_edit_suppressed",
            message_id,
            "Suppressed the expected command response edit"
        );
        return;
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

    let Some(action) = route(authored_by_self, message.text(), runtime) else {
        return;
    };
    tracing::debug!(
        event = "command_matched",
        command = action.name(),
        message_id,
        "Matched authenticated command"
    );

    let response = runtime.execute(client, &action, message_id).await;
    let rendered_text = response.text;
    let input = grammers_client::message::InputMessage::new()
        .text(rendered_text.clone())
        .fmt_entities(response.entities);
    runtime.register_expected_self_edit(peer_id, message_id, rendered_text.clone());
    match message.edit(input).await {
        Ok(()) => {
            tracing::debug!(
                event = "command_edit_succeeded",
                command = action.name(),
                message_id,
                "Edited outgoing command message"
            );
        }
        Err(error) => {
            runtime.remove_expected_self_edit(peer_id, message_id, &rendered_text);
            tracing::warn!(
                event = "command_edit_failed",
                command = action.name(),
                message_id,
                error_category = invocation_error_category(&error),
                error = %error,
                "Failed to edit outgoing command message"
            );
        }
    }
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
    dispatch(&command).or_else(|| runtime.resolve_alias(&command.name, &command.args))
}

#[cfg(test)]
mod tests {
    use grammers_session::types::PeerId;

    use super::{is_self_authored, route};
    use crate::commands::{Action, PrefixRequest};
    use crate::{
        aliases::{Alias, AliasStore},
        runtime::RuntimeState,
        settings::SettingsStore,
    };
    use std::{path::PathBuf, time::Instant};

    async fn runtime() -> RuntimeState {
        RuntimeState::new(
            Instant::now(),
            AliasStore::load(PathBuf::from("/nonexistent/lavis-updates-aliases.json"))
                .await
                .unwrap(),
            SettingsStore::load(PathBuf::from("/nonexistent/lavis-updates-settings.json"))
                .await
                .unwrap(),
            PathBuf::from("/nonexistent/lavis-updates-fastfetch.json"),
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

    #[tokio::test]
    async fn rejects_outgoing_true_messages_not_authored_by_self() {
        let outgoing = true;
        let authored_by_self = false;

        assert!(outgoing);
        assert_eq!(route(authored_by_self, ",ping", &runtime().await), None);
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
        let directory = std::env::temp_dir().join(format!(
            "lavis-updates-prefix-{}-{}",
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
        let runtime = RuntimeState::new(
            Instant::now(),
            aliases,
            settings,
            directory.join("fastfetch.json"),
        );
        assert_eq!(
            route(true, ".help", &runtime),
            Some(Action::Help(crate::commands::HelpRequest::Overview))
        );
        assert_eq!(route(true, ",help", &runtime), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn routes_modules_aliases_and_a_new_prefix_in_the_same_runtime() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-updates-routing-{}-{}",
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
        let mut runtime = RuntimeState::new(
            Instant::now(),
            aliases,
            settings,
            directory.join("fastfetch.json"),
        );

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
    async fn only_suppresses_the_exact_expected_edit_in_its_peer() {
        let mut runtime = runtime().await;
        let first_peer = PeerId::user(1).unwrap();
        let second_peer = PeerId::user(2).unwrap();
        runtime.register_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms".to_owned());

        assert!(!runtime.consume_expected_self_edit(first_peer, 7, ",ping"));
        assert_eq!(route(true, ",ping", &runtime), Some(Action::Ping));
        assert!(!runtime.consume_expected_self_edit(second_peer, 7, "🏓 Pong: 1 ms"));
        assert_eq!(route(true, ",ping", &runtime), Some(Action::Ping));
        assert!(runtime.consume_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms"));
        assert!(!runtime.consume_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms"));
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
}
