use anyhow::Context;
use grammers_client::{client::UpdateStream, update::Update};
use grammers_mtsender::InvocationError;
use grammers_session::types::PeerId;

use crate::{
    command::parse,
    commands::{Action, dispatch},
};

pub async fn run(
    stream: &mut UpdateStream,
    prefix: &str,
    self_user_id: PeerId,
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
                process_update(update, prefix, self_user_id).await;
            }
        }
    }
}

async fn process_update(update: Update, prefix: &str, self_user_id: PeerId) {
    let Update::NewMessage(message) = update else {
        return;
    };
    let message_id = message.id();
    let outgoing = message.outgoing();
    let authored_by_self = is_self_authored(message.sender_id(), outgoing, self_user_id);
    tracing::debug!(
        event = "telegram_new_message",
        message_id,
        outgoing,
        authored_by_self,
        "Received Telegram message update"
    );

    let Some(Action::EditMessage(response)) = route(authored_by_self, message.text(), prefix)
    else {
        return;
    };
    tracing::debug!(
        event = "outgoing_command_matched",
        command = "ping",
        message_id,
        "Matched outgoing command"
    );

    match message.edit(response).await {
        Ok(()) => {
            tracing::debug!(
                event = "command_edit_succeeded",
                command = "ping",
                message_id,
                "Edited outgoing command message"
            );
        }
        Err(error) => {
            tracing::warn!(
                event = "command_edit_failed",
                command = "ping",
                message_id,
                error_category = edit_error_category(&error),
                error = %error,
                "Failed to edit outgoing command message"
            );
        }
    }
}

fn edit_error_category(error: &InvocationError) -> &'static str {
    match error {
        InvocationError::Session(_) => "session",
        InvocationError::Rpc(_) => "rpc",
        InvocationError::Io(_) => "io",
        InvocationError::Deserialize(_) => "deserialize",
        InvocationError::Transport(_) => "transport",
        InvocationError::Dropped => "dropped",
        InvocationError::InvalidDc => "invalid_dc",
        InvocationError::Authentication(_) => "authentication",
    }
}

fn is_self_authored(sender_id: Option<PeerId>, outgoing: bool, self_user_id: PeerId) -> bool {
    match sender_id {
        Some(sender_id) if sender_id == PeerId::self_user() => outgoing,
        Some(sender_id) => sender_id == self_user_id,
        None => false,
    }
}

fn route(authored_by_self: bool, text: &str, prefix: &str) -> Option<Action> {
    authored_by_self
        .then(|| parse(text, prefix))
        .flatten()
        .and_then(|command| dispatch(&command))
}

#[cfg(test)]
mod tests {
    use grammers_session::types::PeerId;

    use super::{is_self_authored, route};
    use crate::commands::Action;

    #[test]
    fn routes_outgoing_false_messages_authored_by_self() {
        let outgoing = false;
        let authored_by_self = true;

        assert!(!outgoing);
        assert_eq!(
            route(authored_by_self, ",ping", ","),
            Some(Action::EditMessage("🏓 Pong!"))
        );
    }

    #[test]
    fn rejects_outgoing_true_messages_not_authored_by_self() {
        let outgoing = true;
        let authored_by_self = false;

        assert!(outgoing);
        assert_eq!(route(authored_by_self, ",ping", ","), None);
    }

    #[test]
    fn ignores_self_authored_normal_unknown_and_dot_prefixed_text() {
        assert_eq!(route(true, "ordinary outgoing text", ","), None);
        assert_eq!(route(true, ",unknown", ","), None);
        assert_eq!(route(true, ".ping", ","), None);
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
