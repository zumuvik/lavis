use std::{path::PathBuf, sync::Arc};

use grammers_client::Client;
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use tokio::{sync::mpsc::UnboundedReceiver, task::JoinHandle};

use crate::{config::Config, error::ClientError};

pub struct TelegramClient {
    client: Client,
    runner: JoinHandle<()>,
    updates: Option<UnboundedReceiver<grammers_session::updates::UpdatesLike>>,
}

impl TelegramClient {
    pub async fn connect(config: &Config) -> Result<Self, ClientError> {
        prepare_session_path(config.session_path.clone()).await?;
        let session = Arc::new(
            SqliteSession::open(&config.session_path)
                .await
                .map_err(|_| ClientError::OpenSession)?,
        );
        secure_session_file(config.session_path.clone()).await?;

        let api_id = i32::try_from(config.api_id).map_err(|_| ClientError::InvalidApiId)?;
        let pool = SenderPool::new(session, api_id);
        let client = Client::new(pool.handle);
        let runner = tokio::spawn(pool.runner.run());

        Ok(Self {
            client,
            runner,
            updates: Some(pool.updates),
        })
    }

    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) fn take_updates(
        &mut self,
    ) -> Result<UnboundedReceiver<grammers_session::updates::UpdatesLike>, ClientError> {
        self.updates.take().ok_or(ClientError::UpdatesAlreadyTaken)
    }

    pub async fn shutdown(self) -> Result<(), ClientError> {
        let Self {
            client,
            runner,
            updates,
        } = self;
        drop(updates);
        client.disconnect();
        drop(client);
        runner.await.map_err(|_| ClientError::RunnerTask)
    }
}

async fn prepare_session_path(session_path: PathBuf) -> Result<(), ClientError> {
    tokio::task::spawn_blocking(move || {
        let parent = session_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or(ClientError::MissingSessionDirectory)?;
        std::fs::create_dir_all(parent).map_err(|_| ClientError::CreateSessionDirectory)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| ClientError::SecureSessionDirectory)?;
        }

        Ok(())
    })
    .await
    .map_err(|_| ClientError::CreateSessionDirectory)?
}

async fn secure_session_file(session_path: PathBuf) -> Result<(), ClientError> {
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(session_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|_| ClientError::SecureSessionFile)?;
        }

        Ok(())
    })
    .await
    .map_err(|_| ClientError::SecureSessionFile)?
}
