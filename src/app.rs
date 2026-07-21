use anyhow::Context;

pub mod auth;
pub mod client;
pub mod command;
pub mod commands;
pub mod config;
pub mod error;
pub mod updates;

pub async fn run() -> anyhow::Result<()> {
    let config = config::Config::load().context("failed to load configuration")?;
    let mut client = client::TelegramClient::connect(&config)
        .await
        .context("failed to open the Telegram session")?;

    let run_result = async {
        let self_user_id = auth::authorize(client.client(), &config)
            .await
            .context("Telegram authorization failed")?;

        initialize_dialog_cache(client.client()).await?;
        let receiver = client
            .take_updates()
            .context("failed to start the Telegram update stream")?;
        let mut stream = client
            .client()
            .stream_updates(
                receiver,
                grammers_client::client::UpdatesConfiguration {
                    catch_up: false,
                    ..Default::default()
                },
            )
            .await
            .map_err(anyhow::Error::from_boxed)
            .context("failed to create the Telegram update stream")?;

        tracing::info!(event = "application_started", "lavis is running");
        updates::run(&mut stream, &config.prefix, self_user_id).await?;
        drop(stream);
        Ok(())
    }
    .await;

    let shutdown_result = client.shutdown().await;
    match (run_result, shutdown_result) {
        (Ok(()), Ok(())) => {
            tracing::info!(event = "application_stopped", "lavis stopped");
            Ok(())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(shutdown_error)) => {
            tracing::error!(
                event = "application_shutdown_failed",
                %shutdown_error,
                "Telegram runner shutdown failed"
            );
            Err(error.context("Telegram runner shutdown also failed"))
        }
    }
}

async fn initialize_dialog_cache(client: &grammers_client::Client) -> anyhow::Result<()> {
    let mut dialogs = client.iter_dialogs();
    while dialogs
        .next()
        .await
        .context("failed to initialize the Telegram dialog cache")?
        .is_some()
    {}
    Ok(())
}
