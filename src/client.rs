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
        let session = Arc::new(match SqliteSession::open(&config.session_path).await {
            Ok(session) => session,
            Err(_) => {
                // grammers does not expose the reason `SqliteSession::open`
                // failed, so a read-only probe distinguishes deterministic
                // corruption (needs manual recovery, exit 78) from transient
                // storage/I/O problems that must not trigger a session wipe.
                let corrupted =
                    probe_session_database_corruption(config.session_path.clone()).await;
                return Err(if corrupted {
                    ClientError::MalformedSession
                } else {
                    ClientError::OpenSession
                });
            }
        });
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

/// Primary SQLite result codes that mean the database file itself is corrupt.
/// The code wrapped by `libsql::Error::SqliteFailure` may be an extended result
/// code, so the primary code is masked out before comparison. These codes are
/// deterministic: retrying cannot recover from them, so they must lead to
/// manual recovery (exit 78) instead of a systemd restart loop.
const SQLITE_CORRUPT: i32 = 11;
const SQLITE_NOTADB: i32 = 26;

fn sqlite_code_indicates_corruption(code: i32) -> bool {
    matches!(code & 0xff, SQLITE_CORRUPT | SQLITE_NOTADB)
}

fn sqlite_error_is_corruption(error: &libsql::Error) -> bool {
    matches!(
        error,
        libsql::Error::SqliteFailure(code, _) if sqlite_code_indicates_corruption(*code)
    )
}

/// Probes the session database once with a read-only SQLite connection.
///
/// `grammers_session` only reports `SqliteSession::open` failures as an opaque
/// error whose type is not nameable from outside the crate, so the distinction
/// cannot be made from its error surface. Instead, a read-only probe of the
/// exact file SQLite will host returns `true` when the database is
/// deterministically corrupt (`SQLITE_CORRUPT`/`SQLITE_NOTADB`) and `false`
/// for transient storage/I/O problems. The probe never modifies the file.
async fn probe_session_database_corruption(path: PathBuf) -> bool {
    let database = match libsql::Builder::new_local(&path)
        .flags(libsql::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .build()
        .await
    {
        Ok(database) => database,
        Err(_) => return false,
    };
    let connection = match database.connect() {
        Ok(connection) => connection,
        Err(_) => return false,
    };
    matches!(
        connection
            .query("SELECT count(*) FROM sqlite_master", libsql::params![])
            .await,
        Err(error) if sqlite_error_is_corruption(&error)
    )
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
    use super::{TelegramClient, validate_session_file_no_follow};
    use crate::error::ClientError;
    use grammers_session::storages::SqliteSession;
    use std::{
        fs,
        os::unix::fs::symlink,
        time::{SystemTime, UNIX_EPOCH},
    };

    /// A file with a valid SQLite magic header but a corrupt body: the
    /// header-based preflight cannot reject it, so the failure surfaces only
    /// inside `SqliteSession::open`. It must be classified as deterministic
    /// corruption (manual recovery / exit 78), never as a retriable open error
    /// that would leave systemd in a restart loop.
    #[tokio::test]
    async fn deep_sqlite_corruption_with_a_valid_header_is_manual_recovery() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-client-session-corrupt-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        let mut contents = b"SQLite format 3\0".to_vec();
        contents.extend_from_slice(&[0x10, 0x00]); // page size 4096, big-endian
        contents.extend_from_slice(&[0x01, 0x01, 0x00]); // write/read version, reserved space
        contents.extend_from_slice(&[0u8; 2048]);
        fs::write(&session, &contents).unwrap();

        assert!(
            !crate::session::has_invalid_sqlite_header(&session).unwrap(),
            "precondition: the validity check only sees the header"
        );
        if (SqliteSession::open(&session).await).is_ok() {
            panic!("a corrupt database must fail to open");
        }
        assert!(
            super::probe_session_database_corruption(session.clone()).await,
            "the read-only probe must classify deep corruption as malformed"
        );
        // Upper-level semantics tested without a real process::exit: the same
        // MalformedSession classification that `requires_manual_recovery`
        // inspects in main.rs leads to exit status 78 there.
        let wrapped = anyhow::Error::new(ClientError::MalformedSession)
            .context("failed to open the Telegram session");
        assert!(
            crate::requires_manual_recovery(&wrapped),
            "deep corruption must terminate the service (exit 78) instead of restart-looping"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    /// End-to-end proof of the classifier wiring: a session that only deep
    /// corruption exposes (valid header, garbage body) must surface from the
    /// real `TelegramClient::connect` path as `MalformedSession` — not as a
    /// transient `OpenSession` that would let systemd restart-loop. This is
    /// the exact file for which neither `Builder::build()` nor its `connect()`
    /// can produce `SQLITE_CORRUPT`/`SQLITE_NOTADB` early: SQLite defers all
    /// database-content validation until the first query, which is precisely
    /// where the probe classifies corruption.
    #[tokio::test]
    async fn connect_classifies_a_corrupt_session_as_manual_recovery() {
        use crate::config::{API_HASH_ENV, API_ID_ENV, Config, ConfigPaths};
        use std::ffi::OsString;

        let directory = std::env::temp_dir().join(format!(
            "lavis-client-connect-corrupt-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        let mut contents = b"SQLite format 3\0".to_vec();
        contents.extend_from_slice(&[0x10, 0x00]); // page size 4096, big-endian
        contents.extend_from_slice(&[0x01, 0x01, 0x00]); // write/read version, reserved space
        contents.extend_from_slice(&[0u8; 2048]);
        fs::write(&session, &contents).unwrap();

        let environment = |name: &str| match name {
            API_ID_ENV => Some(OsString::from("12345")),
            API_HASH_ENV => Some(OsString::from("test-api-hash")),
            _ => None,
        };
        let config = Config::load_with(&environment, ConfigPaths::new(&session)).unwrap();

        let error = match TelegramClient::connect(&config).await {
            Ok(_) => panic!("a corrupt session must not connect"),
            Err(error) => error,
        };
        assert!(
            matches!(error, ClientError::MalformedSession),
            "deep corruption must map to MalformedSession, not OpenSession"
        );
        let wrapped = anyhow::Error::new(error);
        assert!(
            crate::requires_manual_recovery(&wrapped),
            "the error produced by the real connect path must terminate (exit 78)"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn a_valid_session_database_is_not_classified_as_corruption() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-client-session-valid-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let session = directory.join("session");
        let created = SqliteSession::open(&session).await.unwrap();
        drop(created);

        assert!(
            !super::probe_session_database_corruption(session.clone()).await,
            "a freshly created session database must probe clean"
        );
        fs::remove_dir_all(directory).unwrap();
    }

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
