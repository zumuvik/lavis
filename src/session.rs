use std::{
    fs, io,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use rustix::{
    fs::{CWD, FlockOperation, Mode, OFlags, fchmod, flock, openat},
    io::Errno,
};

use crate::error::{ClientError, LastAuthorizationDiagnostic};

/// An exclusive advisory lock for the local Telegram session.
///
/// The file is deliberately retained after release: the open descriptor owns
/// the lock, while the stable path prevents replacement races between users.
pub(crate) struct SessionLock {
    _file: fs::File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionLockState {
    Locked(Option<SessionLockHolder>),
    Unlocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionLockHolder {
    pub(crate) pid: u32,
    pub(crate) context: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionLockContext {
    Client,
    Doctor,
    Reset,
    Logout,
}

impl SessionLockContext {
    fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Doctor => "doctor",
            Self::Reset => "reset",
            Self::Logout => "logout",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoredAuthorizationDiagnostic {
    Absent,
    Present(LastAuthorizationDiagnostic),
    Invalid,
}

impl SessionLock {
    pub(crate) fn acquire(
        session_path: &Path,
        context: SessionLockContext,
    ) -> Result<Self, ClientError> {
        let lock_path = lock_path(session_path);
        let file = openat(
            CWD,
            &lock_path,
            OFlags::CREATE | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| ClientError::OpenSessionLock)?;

        let mut file = fs::File::from(file);
        fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(|_| ClientError::SecureSessionLock)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {
                write_lock_holder(&mut file, context)?;
                Ok(Self { _file: file })
            }
            Err(Errno::AGAIN) => Err(ClientError::SessionLocked),
            Err(_) => Err(ClientError::LockSession),
        }
    }
}

pub(crate) fn lock_state(session_path: &Path) -> Result<SessionLockState, ClientError> {
    match fs::symlink_metadata(lock_path(session_path)) {
        Ok(_) => match SessionLock::acquire(session_path, SessionLockContext::Doctor) {
            Ok(lock) => {
                drop(lock);
                Ok(SessionLockState::Unlocked)
            }
            Err(ClientError::SessionLocked) => {
                Ok(SessionLockState::Locked(read_lock_holder(session_path)))
            }
            Err(error) => Err(error),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(SessionLockState::Unlocked),
        Err(_) => Err(ClientError::InspectSession),
    }
}

fn write_lock_holder(file: &mut fs::File, context: SessionLockContext) -> Result<(), ClientError> {
    file.set_len(0).map_err(|_| ClientError::LockSession)?;
    file.write_all(format!("pid={} context={}\n", std::process::id(), context.as_str()).as_bytes())
        .map_err(|_| ClientError::LockSession)?;
    file.sync_data().map_err(|_| ClientError::LockSession)
}

fn read_lock_holder(session_path: &Path) -> Option<SessionLockHolder> {
    let file = openat(
        CWD,
        lock_path(session_path),
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .ok()?;
    let mut contents = String::new();
    fs::File::from(file)
        .take(128)
        .read_to_string(&mut contents)
        .ok()?;
    let (pid, context) = contents.trim_end().split_once(" context=")?;
    let pid = pid.strip_prefix("pid=")?.parse().ok()?;
    let context = match context {
        "client" => "client",
        "doctor" => "doctor",
        "reset" => "reset",
        "logout" => "logout",
        _ => return None,
    };
    Some(SessionLockHolder { pid, context })
}

pub(crate) fn write_last_authorization_diagnostic(
    session_path: &Path,
    diagnostic: LastAuthorizationDiagnostic,
) -> Result<(), ClientError> {
    let temporary_path = authorization_diagnostic_temporary_path(session_path);
    let file = openat(
        CWD,
        &temporary_path,
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|_| ClientError::WriteAuthorizationDiagnostic)?;
    fchmod(&file, Mode::RUSR | Mode::WUSR)
        .map_err(|_| ClientError::WriteAuthorizationDiagnostic)?;
    let mut file = fs::File::from(file);
    file.write_all(format!("v1:{}", diagnostic.category()).as_bytes())
        .map_err(|_| ClientError::WriteAuthorizationDiagnostic)?;
    file.write_all(b"\n")
        .map_err(|_| ClientError::WriteAuthorizationDiagnostic)?;
    file.sync_all()
        .map_err(|_| ClientError::WriteAuthorizationDiagnostic)?;
    drop(file);
    fs::rename(temporary_path, authorization_diagnostic_path(session_path))
        .map_err(|_| ClientError::WriteAuthorizationDiagnostic)
}

pub(crate) fn read_last_authorization_diagnostic(
    session_path: &Path,
) -> Result<StoredAuthorizationDiagnostic, ClientError> {
    let path = authorization_diagnostic_path(session_path);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(StoredAuthorizationDiagnostic::Absent);
        }
        Err(_) => return Err(ClientError::ReadAuthorizationDiagnostic),
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Ok(StoredAuthorizationDiagnostic::Invalid);
        }
        Ok(_) => {}
    }
    let file = openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| ClientError::ReadAuthorizationDiagnostic)?;
    let mut contents = Vec::new();
    fs::File::from(file)
        .take(128)
        .read_to_end(&mut contents)
        .map_err(|_| ClientError::ReadAuthorizationDiagnostic)?;
    match std::str::from_utf8(&contents)
        .ok()
        .and_then(|contents| contents.strip_suffix('\n'))
        .and_then(|contents| contents.strip_prefix("v1:"))
        .and_then(LastAuthorizationDiagnostic::from_category)
    {
        Some(diagnostic) => Ok(StoredAuthorizationDiagnostic::Present(diagnostic)),
        None => Ok(StoredAuthorizationDiagnostic::Invalid),
    }
}

pub(crate) fn has_invalid_sqlite_header(session_path: &Path) -> Result<bool, ClientError> {
    const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";
    let metadata = match fs::symlink_metadata(session_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(ClientError::InspectSession),
        Ok(metadata) => metadata,
    };
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        return Ok(false);
    }
    let file = openat(
        CWD,
        session_path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| ClientError::InspectSession)?;
    let mut header = [0_u8; SQLITE_HEADER.len()];
    match fs::File::from(file).read_exact(&mut header) {
        Ok(()) => Ok(header != SQLITE_HEADER),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(true),
        Err(_) => Err(ClientError::InspectSession),
    }
}

pub(crate) fn validate_session_file_no_follow(session_path: &Path) -> Result<(), ClientError> {
    match fs::symlink_metadata(session_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(ClientError::OpenSession),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(ClientError::SessionSymlink);
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(ClientError::MalformedSession);
        }
        Ok(_) => {}
    }
    let file = openat(
        CWD,
        session_path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| ClientError::SessionSymlink)?;
    drop(file);
    if has_invalid_sqlite_header(session_path)? {
        Err(ClientError::MalformedSession)
    } else {
        Ok(())
    }
}

fn lock_path(session_path: &Path) -> PathBuf {
    session_path.with_extension("lock")
}

fn authorization_diagnostic_path(session_path: &Path) -> PathBuf {
    session_path.with_extension("auth-diagnostic")
}

fn authorization_diagnostic_temporary_path(session_path: &Path) -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut path = authorization_diagnostic_path(session_path);
    path.as_mut_os_string()
        .push(format!(".tmp-{}-{sequence}", std::process::id()));
    path
}

#[cfg(test)]
mod tests {
    use super::{
        SessionLock, SessionLockContext, SessionLockHolder, SessionLockState,
        StoredAuthorizationDiagnostic, lock_path, lock_state, read_last_authorization_diagnostic,
        write_last_authorization_diagnostic,
    };
    use crate::error::{ClientError, LastAuthorizationDiagnostic};
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

        let first = SessionLock::acquire(&session_path, SessionLockContext::Client).unwrap();
        assert!(matches!(
            SessionLock::acquire(&session_path, SessionLockContext::Client),
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
        let second = SessionLock::acquire(&session_path, SessionLockContext::Client).unwrap();
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
            SessionLock::acquire(&session_path, SessionLockContext::Client),
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
        let lock = SessionLock::acquire(&session_path, SessionLockContext::Client).unwrap();

        assert_eq!(
            lock_state(&session_path).unwrap(),
            SessionLockState::Locked(Some(SessionLockHolder {
                pid: std::process::id(),
                context: "client",
            }))
        );
        drop(lock);
        assert_eq!(
            lock_state(&session_path).unwrap(),
            SessionLockState::Unlocked
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_only_a_safe_last_authorization_category() {
        let directory = test_directory("diagnostic");
        fs::create_dir(&directory).unwrap();
        let session_path = directory.join("session");

        write_last_authorization_diagnostic(
            &session_path,
            LastAuthorizationDiagnostic::SessionStorage,
        )
        .unwrap();
        assert_eq!(
            read_last_authorization_diagnostic(&session_path).unwrap(),
            StoredAuthorizationDiagnostic::Present(LastAuthorizationDiagnostic::SessionStorage)
        );
        assert_eq!(
            fs::read_to_string(directory.join("session.auth-diagnostic")).unwrap(),
            "v1:session_storage\n"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_malformed_persisted_authorization_diagnostics() {
        let directory = test_directory("diagnostic-invalid");
        fs::create_dir(&directory).unwrap();
        let session_path = directory.join("session");
        fs::write(
            directory.join("session.auth-diagnostic"),
            "api_hash=secret\n",
        )
        .unwrap();

        assert_eq!(
            read_last_authorization_diagnostic(&session_path).unwrap(),
            StoredAuthorizationDiagnostic::Invalid
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn detects_non_sqlite_session_headers() {
        let directory = test_directory("sqlite-header");
        fs::create_dir(&directory).unwrap();
        let session_path = directory.join("session");
        fs::write(&session_path, "not a SQLite database").unwrap();

        assert!(super::has_invalid_sqlite_header(&session_path).unwrap());
        fs::write(&session_path, b"SQLite format 3\0remaining database data").unwrap();
        assert!(!super::has_invalid_sqlite_header(&session_path).unwrap());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subprocess_lock_contention_is_reported() {
        let directory = test_directory("subprocess");
        fs::create_dir(&directory).unwrap();
        let session_path = directory.join("session");
        let lock_path = lock_path(&session_path);
        let mut child = std::process::Command::new("sh")
            .args([
                "-c",
                "exec 9>\"$1\"; flock -n 9; sleep 1",
                "sh",
                lock_path.to_str().unwrap(),
            ])
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(matches!(
            SessionLock::acquire(&session_path, SessionLockContext::Client),
            Err(ClientError::SessionLocked)
        ));
        child.wait().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }
}
