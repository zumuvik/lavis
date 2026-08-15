use std::{
    fs, io,
    path::{Path, PathBuf},
};

use rustix::{
    fd::OwnedFd,
    fs::{CWD, FlockOperation, Mode, OFlags, fchmod, flock, openat},
    io::Errno,
};

use crate::error::ClientError;

/// An exclusive advisory lock for the local Telegram session.
///
/// The file is deliberately retained after release: the open descriptor owns
/// the lock, while the stable path prevents replacement races between users.
pub(crate) struct SessionLock {
    _file: OwnedFd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionLockState {
    Locked,
    Unlocked,
}

impl SessionLock {
    pub(crate) fn acquire(session_path: &Path) -> Result<Self, ClientError> {
        let lock_path = lock_path(session_path);
        let file = openat(
            CWD,
            &lock_path,
            OFlags::CREATE | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| ClientError::OpenSessionLock)?;

        fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(|_| ClientError::SecureSessionLock)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self { _file: file }),
            Err(Errno::AGAIN) => Err(ClientError::SessionLocked),
            Err(_) => Err(ClientError::LockSession),
        }
    }
}

pub(crate) fn lock_state(session_path: &Path) -> Result<SessionLockState, ClientError> {
    match fs::symlink_metadata(lock_path(session_path)) {
        Ok(_) => match SessionLock::acquire(session_path) {
            Ok(lock) => {
                drop(lock);
                Ok(SessionLockState::Unlocked)
            }
            Err(ClientError::SessionLocked) => Ok(SessionLockState::Locked),
            Err(error) => Err(error),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(SessionLockState::Unlocked),
        Err(_) => Err(ClientError::InspectSession),
    }
}

fn lock_path(session_path: &Path) -> PathBuf {
    session_path.with_extension("lock")
}

#[cfg(test)]
mod tests {
    use super::{SessionLock, SessionLockState, lock_path, lock_state};
    use crate::error::ClientError;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        time::{SystemTime, UNIX_EPOCH},
    };

    fn test_directory(label: &str) -> std::path::PathBuf {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "lavis-session-lock-{label}-{}-{sequence}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn exclusive_lock_rejects_a_second_acquisition_and_releases_on_drop() {
        let directory = test_directory("exclusive");
        fs::create_dir(&directory).unwrap();
        let session_path = directory.join("session");

        let first = SessionLock::acquire(&session_path).unwrap();
        assert!(matches!(
            SessionLock::acquire(&session_path),
            Err(ClientError::SessionLocked)
        ));
        assert_eq!(
            fs::metadata(lock_path(&session_path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        drop(first);
        let second = SessionLock::acquire(&session_path).unwrap();
        drop(second);
        assert!(lock_path(&session_path).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refuses_a_symlinked_lock_file() {
        let directory = test_directory("symlink");
        fs::create_dir(&directory).unwrap();
        let session_path = directory.join("session");
        let target = directory.join("target");
        fs::write(&target, "unrelated").unwrap();
        symlink(&target, lock_path(&session_path)).unwrap();

        assert!(matches!(
            SessionLock::acquire(&session_path),
            Err(ClientError::OpenSessionLock)
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "unrelated");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reports_existing_lock_contention_without_modifying_the_lock() {
        let directory = test_directory("state");
        fs::create_dir(&directory).unwrap();
        let session_path = directory.join("session");
        let lock = SessionLock::acquire(&session_path).unwrap();

        assert_eq!(lock_state(&session_path).unwrap(), SessionLockState::Locked);
        drop(lock);
        assert_eq!(
            lock_state(&session_path).unwrap(),
            SessionLockState::Unlocked
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
