use std::{path::PathBuf, sync::Arc};

use grammers_client::{
    Client,
    client::{ClientConfiguration, NoRetries},
};
use grammers_mtsender::{SenderPool, SenderPoolFatHandle};
use grammers_session::storages::SqliteSession;
use tokio::{sync::mpsc::UnboundedReceiver, task::JoinHandle};

use crate::{
    config::Config,
    error::ClientError,
    session::{SessionLock, SessionLockContext},
};

/// Dedicated Telegram transport exposed to the module RPC executor.
///
/// `client` is retained for the curated typed helpers. `raw_handle` exposes the
/// sender-pool byte transport used by the explicit `raw.invoke` escape hatch;
/// modules never receive either handle directly.
pub struct ModuleRpcClient {
    pub(crate) client: Client,
    pub(crate) raw_handle: SenderPoolFatHandle,
}

pub struct TelegramClient {
    client: Client,
    module_rpc_handle: SenderPoolFatHandle,
    runner: JoinHandle<()>,
    updates: Option<UnboundedReceiver<grammers_session::updates::UpdatesLike>>,
    session_lock: SessionLock,
}

impl TelegramClient {
    pub async fn connect(config: &Config) -> Result<Self, ClientError> {
        prepare_session_path(config.session_path.clone()).await?;
        let session_lock = SessionLock::acquire(&config.session_path, SessionLockContext::Client)?;
        validate_session_file_no_follow(config.session_path.clone()).await?;
        let session = Arc::new(
            SqliteSession::open(&config.session_path)
                .await
                .map_err(|_| ClientError::OpenSession)?,
        );
        secure_session_file(config.session_path.clone()).await?;

        let api_id = i32::try_from(config.api_id).map_err(|_| ClientError::InvalidApiId)?;
        let pool = SenderPool::new(session, api_id);
        let module_rpc_handle = pool.handle.clone();
        let client = Client::new(pool.handle);
        let runner = tokio::spawn(pool.runner.run());

        Ok(Self {
            client,
            module_rpc_handle,
            runner,
            updates: Some(pool.updates),
            session_lock,
        })
    }

    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) fn module_rpc_client(&self) -> ModuleRpcClient {
        let raw_handle = self.module_rpc_handle.clone();
        let client = Client::with_configuration(
            raw_handle.clone(),
            ClientConfiguration {
                retry_policy: Box::new(NoRetries),
                auto_cache_peers: false,
            },
        );
        ModuleRpcClient { client, raw_handle }
    }

    pub(crate) fn take_updates(
        &mut self,
    ) -> Result<UnboundedReceiver<grammers_session::updates::UpdatesLike>, ClientError> {
        self.updates.take().ok_or(ClientError::UpdatesAlreadyTaken)
    }

    pub async fn shutdown(self) -> Result<(), ClientError> {
        let Self {
            client,
            module_rpc_handle,
            runner,
            updates,
            session_lock,
        } = self;
        drop(updates);
        drop(module_rpc_handle);
        client.disconnect();
        drop(client);
        let result = runner.await.map_err(|_| ClientError::RunnerTask);
        drop(session_lock);
        result
    }
}

async fn validate_session_file_no_follow(session_path: PathBuf) -> Result<(), ClientError> {
    tokio::task::spawn_blocking(move || {
        crate::session::validate_session_file_no_follow(&session_path)
    })
    .await
    .map_err(|_| ClientError::OpenSession)?
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

#[cfg(test)]
mod tests {
    use super::validate_session_file_no_follow;
    use crate::error::ClientError;
    use std::{
        fs,
        os::unix::fs::symlink,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[tokio::test]
    async fn rejects_a_symlink_before_opening_the_session_database() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-client-session-symlink-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let target = directory.join("target");
        fs::write(&target, "unrelated").unwrap();
        let session = directory.join("session");
        symlink(&target, &session).unwrap();

        assert!(matches!(
            validate_session_file_no_follow(session).await,
            Err(ClientError::SessionSymlink)
        ));
        assert_eq!(fs::read_to_string(target).unwrap(), "unrelated");
        fs::remove_dir_all(directory).unwrap();
    }
}
