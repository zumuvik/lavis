use super::MAX_ENABLED_MODULES;
use crate::error::ExternalError;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const VERSION: u32 = 1;
const MAX_FILE_BYTES: usize = 4096;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    version: u32,
    enabled: Vec<String>,
}

#[derive(Debug)]
pub struct ExternalStateStore {
    path: PathBuf,
    enabled: BTreeSet<String>,
    temporary_counter: u64,
}

impl ExternalStateStore {
    pub fn new_disabled() -> Self {
        Self {
            path: PathBuf::from("/dev/null"),
            enabled: BTreeSet::new(),
            temporary_counter: 0,
        }
    }

    pub async fn load(path: PathBuf) -> Result<Self, ExternalError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|_| ExternalError::StateRead)?;
        }

        let enabled = tokio::task::spawn_blocking({
            let path = path.clone();
            move || load_state(&path)
        })
        .await
        .map_err(|_| ExternalError::StateRead)??;

        Ok(Self {
            path,
            enabled,
            temporary_counter: 0,
        })
    }

    pub fn enabled_ids(&self) -> &BTreeSet<String> {
        &self.enabled
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.enabled.contains(id)
    }

    pub async fn enable(&mut self, id: &str) -> Result<(), ExternalError> {
        crate::external_modules::manifest::validate_module_id(id)?;
        if self.enabled.contains(id) {
            return Ok(());
        }
        let mut candidate = self.enabled.clone();
        if candidate.len() >= MAX_ENABLED_MODULES {
            return Err(ExternalError::ModuleLimit);
        }
        candidate.insert(id.to_owned());
        self.commit(candidate).await
    }

    pub async fn disable(&mut self, id: &str) -> Result<(), ExternalError> {
        crate::external_modules::manifest::validate_module_id(id)?;
        let mut candidate = self.enabled.clone();
        candidate.remove(id);
        if candidate.len() == self.enabled.len() {
            return Err(ExternalError::NotEnabled);
        }
        self.commit(candidate).await
    }

    pub async fn disable_idempotent(&mut self, id: &str) -> Result<bool, ExternalError> {
        crate::external_modules::manifest::validate_module_id(id)?;
        if !self.enabled.contains(id) {
            return Ok(false);
        }
        let mut candidate = self.enabled.clone();
        candidate.remove(id);
        self.commit(candidate).await?;
        Ok(true)
    }

    async fn commit(&mut self, candidate: BTreeSet<String>) -> Result<(), ExternalError> {
        let ids: Vec<String> = candidate.iter().cloned().collect();
        let bytes = serialize_state(&ids)?;
        self.temporary_counter = self.temporary_counter.saturating_add(1);
        tokio::task::spawn_blocking({
            let path = self.path.clone();
            let bytes = bytes.clone();
            let counter = self.temporary_counter;
            move || write_state(&path, &bytes, counter)
        })
        .await
        .map_err(|_| ExternalError::StateWrite)??;
        self.enabled = candidate;
        Ok(())
    }
}

fn load_state(path: &Path) -> Result<BTreeSet<String>, ExternalError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(_) => return Err(ExternalError::StateRead),
    };
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ExternalError::StateRead)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ExternalError::StateFileTooLarge);
    }
    let state: StateFile =
        serde_json::from_slice(&bytes).map_err(|_| ExternalError::StateMalformed)?;
    if state.version != VERSION {
        return Err(ExternalError::StateUnsupportedVersion);
    }
    let mut ids = BTreeSet::new();
    for id in &state.enabled {
        crate::external_modules::manifest::validate_module_id(id)
            .map_err(|_| ExternalError::StateMalformed)?;
        if !ids.insert(id.clone()) {
            return Err(ExternalError::StateMalformed);
        }
    }
    if ids.len() > MAX_ENABLED_MODULES {
        return Err(ExternalError::StateMalformed);
    }
    Ok(ids)
}

fn serialize_state(ids: &[String]) -> Result<Vec<u8>, ExternalError> {
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "version": VERSION,
        "enabled": ids,
    }))
    .map_err(|_| ExternalError::StateWrite)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ExternalError::StateFileTooLarge);
    }
    Ok(bytes)
}

fn write_state(path: &Path, bytes: &[u8], counter: u64) -> Result<(), ExternalError> {
    let parent = path.parent().ok_or(ExternalError::StateWrite)?;
    if !parent.is_dir() {
        return Err(ExternalError::StateWrite);
    }
    let tmp = parent.join(format!(
        ".external-modules.json.{}.{}.tmp",
        std::process::id(),
        counter
    ));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp).map_err(|_| ExternalError::StateWrite)?;
    if file.write_all(bytes).is_err() || file.flush().is_err() {
        let _ = fs::remove_file(&tmp);
        return Err(ExternalError::StateWrite);
    }
    if file.sync_all().is_err() {
        let _ = fs::remove_file(&tmp);
        return Err(ExternalError::StateWrite);
    }
    drop(file);
    if fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
        return Err(ExternalError::StateWrite);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn path() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("lavis-ext-state-{nonce}-{seq}"));
        fs::create_dir_all(&d).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        }
        d.join("external-modules.json")
    }

    #[tokio::test]
    async fn missing_state_returns_empty() {
        let p = path();
        let store = ExternalStateStore::load(p.clone()).await.unwrap();
        assert!(store.enabled_ids().is_empty());
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn enable_and_disable_roundtrip() {
        let p = path();
        let mut store = ExternalStateStore::load(p.clone()).await.unwrap();
        store.enable("echo").await.unwrap();
        assert!(store.is_enabled("echo"));
        store.disable("echo").await.unwrap();
        assert!(!store.is_enabled("echo"));
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn disable_not_enabled_returns_error() {
        let p = path();
        let mut store = ExternalStateStore::load(p.clone()).await.unwrap();
        assert!(store.disable("echo").await.is_err());
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn malformed_file_preserved() {
        let p = path();
        fs::write(&p, "not json").unwrap();
        assert!(ExternalStateStore::load(p.clone()).await.is_err());
        assert_eq!(fs::read_to_string(&p).unwrap(), "not json");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn unknown_fields_rejected() {
        let p = path();
        fs::write(&p, r#"{"version":1,"enabled":[],"extra":true}"#).unwrap();
        assert!(matches!(
            ExternalStateStore::load(p.clone()).await,
            Err(ExternalError::StateMalformed)
        ));
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn unsupported_version_preserved() {
        let p = path();
        fs::write(&p, r#"{"version":2,"enabled":[]}"#).unwrap();
        assert!(matches!(
            ExternalStateStore::load(p.clone()).await,
            Err(ExternalError::StateUnsupportedVersion)
        ));
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            r#"{"version":2,"enabled":[]}"#
        );
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn invalid_id_in_file_rejected() {
        let p = path();
        fs::write(&p, r#"{"version":1,"enabled":["BAD_ID"]}"#).unwrap();
        assert!(ExternalStateStore::load(p.clone()).await.is_err());
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn duplicate_ids_rejected() {
        let p = path();
        fs::write(&p, r#"{"version":1,"enabled":["echo","echo"]}"#).unwrap();
        assert!(ExternalStateStore::load(p.clone()).await.is_err());
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn max_count_enforced() {
        let p = path();
        let mut store = ExternalStateStore::load(p.clone()).await.unwrap();
        for i in 0..MAX_ENABLED_MODULES {
            store.enable(&format!("mod{i}")).await.unwrap();
        }
        assert!(store.enable("extra").await.is_err());
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn enable_idempotent() {
        let p = path();
        let mut store = ExternalStateStore::load(p.clone()).await.unwrap();
        store.enable("echo").await.unwrap();
        assert!(store.enable("echo").await.is_ok());
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn disable_idempotent_returns_false_for_not_enabled() {
        let p = path();
        let mut store = ExternalStateStore::load(p.clone()).await.unwrap();
        assert!(!store.disable_idempotent("echo").await.unwrap());
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }
}
