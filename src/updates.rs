use anyhow::Context;
use grammers_client::{client::UpdateStream, update::Update};

use crate::{
    command::parse,
    commands::{Action, dispatch},
};

pub async fn run(stream: &mut UpdateStream, prefix: &str) -> anyhow::Result<()> {
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
                process_update(update, prefix).await;
            }
        }
    }
}

async fn process_update(update: Update, prefix: &str) {
    let Update::NewMessage(message) = update else {
        return;
    };
    let Some(Action::EditMessage(response)) = route(message.outgoing(), message.text(), prefix)
    else {
        return;
    };
    let message_id = message.id();

    if message.edit(response).await.is_err() {
        tracing::warn!(
            event = "command_edit_failed",
            command = "ping",
            message_id,
            "Failed to edit outgoing command message"
        );
    }
}

fn route(outgoing: bool, text: &str, prefix: &str) -> Option<Action> {
    outgoing
        .then(|| parse(text, prefix))
        .flatten()
        .and_then(|command| dispatch(&command))
}

#[cfg(test)]
mod tests {
    use super::route;
    use crate::commands::Action;

    #[test]
    fn ignores_incoming_command_text() {
        assert_eq!(route(false, ",ping", ","), None);
    }

    #[test]
    fn ignores_ordinary_outgoing_text() {
        assert_eq!(route(true, "ordinary outgoing text", ","), None);
    }

    #[test]
    fn routes_outgoing_ping_to_an_edit() {
        assert_eq!(
            route(true, ",ping", ","),
            Some(Action::EditMessage("🏓 Pong!"))
        );
    }
}
