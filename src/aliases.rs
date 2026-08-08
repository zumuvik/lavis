use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{commands::canonical_command, error::AliasError};

const VERSION: u32 = 1;
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_ALIASES: usize = 128;
const MAX_ARGS: usize = 32;
const MAX_ARG_BYTES: usize = 256;
const MAX_ARGS_BYTES: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alias {
    pub target: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasInvocation {
    pub target: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddResult {
    Added,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteResult {
    Deleted,
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AliasFile {
    version: u32,
    aliases: BTreeMap<String, Alias>,
}

pub struct AliasStore {
    path: PathBuf,
    aliases: BTreeMap<String, Alias>,
    temporary_counter: u64,
}

impl AliasStore {
    pub async fn load(path: PathBuf) -> Result<Self, AliasError> {
        let aliases = tokio::task::spawn_blocking({
            let path = path.clone();
            move || load_aliases(&path)
        })
        .await
        .map_err(|_| AliasError::StorageTask)??;

        Ok(Self {
            path,
            aliases,
            temporary_counter: 0,
        })
    }

    pub fn aliases(&self) -> &BTreeMap<String, Alias> {
        &self.aliases
    }

    pub fn lookup(&self, name: &str) -> Option<&Alias> {
        normalize_name(name)
            .ok()
            .and_then(|name| self.aliases.get(&name))
    }

    pub fn invocation(
        &self,
        name: &str,
        invocation_args: &[String],
    ) -> Result<Option<AliasInvocation>, AliasError> {
        let Some(alias) = self.lookup(name) else {
            return Ok(None);
        };
        let mut args = alias.args.clone();
        args.extend_from_slice(invocation_args);
        validate_args(&args)?;

        Ok(Some(AliasInvocation {
            target: alias.target.clone(),
            args,
        }))
    }

    pub async fn add(&mut self, name: &str, alias: Alias) -> Result<AddResult, AliasError> {
        let name = normalize_name(name)?;
        if self.aliases.contains_key(&name) {
            return Err(AliasError::AlreadyExists);
        }
        validate_alias(&name, &alias)?;

        let mut candidate = self.aliases.clone();
        candidate.insert(name, alias);
        validate_aliases(&candidate)?;
        self.commit(candidate).await?;
        Ok(AddResult::Added)
    }

    pub async fn delete(&mut self, name: &str) -> Result<DeleteResult, AliasError> {
        let name = normalize_name(name)?;
        let mut candidate = self.aliases.clone();
        if candidate.remove(&name).is_none() {
            return Ok(DeleteResult::NotFound);
        }
        self.commit(candidate).await?;
        Ok(DeleteResult::Deleted)
    }

    async fn commit(&mut self, candidate: BTreeMap<String, Alias>) -> Result<(), AliasError> {
        let bytes = serialize_aliases(&candidate)?;
        self.temporary_counter = self.temporary_counter.saturating_add(1);
        let directory_synced = tokio::task::spawn_blocking({
            let path = self.path.clone();
            let temporary_counter = self.temporary_counter;
            move || write_aliases(&path, &bytes, temporary_counter)
        })
        .await
        .map_err(|_| AliasError::StorageTask)??;

        self.aliases = candidate;
        if !directory_synced {
            tracing::warn!(
                event = "alias_directory_sync_failed",
                "Alias file was committed but directory synchronization failed"
            );
        }
        Ok(())
    }
}

fn load_aliases(path: &Path) -> Result<BTreeMap<String, Alias>, AliasError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => return Err(AliasError::Read),
    };
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| AliasError::Read)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(AliasError::FileTooLarge);
    }
    let alias_file: AliasFile =
        serde_json::from_slice(&bytes).map_err(|_| AliasError::MalformedFile)?;
    if alias_file.version != VERSION {
        return Err(AliasError::UnsupportedVersion);
    }
    validate_aliases(&alias_file.aliases)?;
    Ok(alias_file.aliases)
}

fn serialize_aliases(aliases: &BTreeMap<String, Alias>) -> Result<Vec<u8>, AliasError> {
    let bytes = serde_json::to_vec_pretty(&AliasFile {
        version: VERSION,
        aliases: aliases.clone(),
    })
    .map_err(|_| AliasError::WriteTemporary)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(AliasError::FileTooLarge);
    }
    Ok(bytes)
}

fn validate_aliases(aliases: &BTreeMap<String, Alias>) -> Result<(), AliasError> {
    if aliases.len() > MAX_ALIASES {
        return Err(AliasError::AliasLimit);
    }
    for (name, alias) in aliases {
        if normalize_name(name)? != *name {
            return Err(AliasError::InvalidName);
        }
        validate_alias(name, alias)?;
    }
    Ok(())
}

fn validate_alias(name: &str, alias: &Alias) -> Result<(), AliasError> {
    if name == "prefix" {
        return Err(AliasError::PrefixReserved);
    }
    if name == "modules" {
        return Err(AliasError::ModulesReserved);
    }
    if canonical_command(name).is_some() {
        return Err(AliasError::ReservedName);
    }
    let target = canonical_command(&alias.target).ok_or(AliasError::UnknownTarget)?;
    if !target.aliasable {
        return Err(AliasError::TargetNotAliasable);
    }
    validate_args(&alias.args)
}

fn validate_args(args: &[String]) -> Result<(), AliasError> {
    if args.len() > MAX_ARGS {
        return Err(AliasError::ArgumentLimit);
    }
    let mut bytes = 0usize;
    for arg in args {
        if arg.len() > MAX_ARG_BYTES {
            return Err(AliasError::ArgumentTooLong);
        }
        bytes = bytes
            .checked_add(arg.len())
            .ok_or(AliasError::ArgumentsTooLarge)?;
    }
    if bytes > MAX_ARGS_BYTES {
        return Err(AliasError::ArgumentsTooLarge);
    }
    Ok(())
}

fn normalize_name(name: &str) -> Result<String, AliasError> {
    let name = name.to_ascii_lowercase();
    let bytes = name.as_bytes();
    if !(1..=32).contains(&bytes.len())
        || !bytes[0].is_ascii_lowercase()
        || !bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        return Err(AliasError::InvalidName);
    }
    Ok(name)
}

fn write_aliases(path: &Path, bytes: &[u8], temporary_counter: u64) -> Result<bool, AliasError> {
    let parent = path.parent().ok_or(AliasError::CreateDirectory)?;
    if !parent.is_dir() {
        return Err(AliasError::CreateDirectory);
    }

    let temporary_path = parent.join(format!(
        ".aliases.json.{}.{}.tmp",
        std::process::id(),
        temporary_counter
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary_path)
        .map_err(|_| AliasError::CreateTemporary)?;
    if file.write_all(bytes).is_err() || file.flush().is_err() {
        drop(file);
        return Err(cleanup_after_failure(
            &temporary_path,
            AliasError::WriteTemporary,
        ));
    }
    if file.sync_all().is_err() {
        drop(file);
        return Err(cleanup_after_failure(
            &temporary_path,
            AliasError::SyncTemporary,
        ));
    }
    drop(file);
    if fs::rename(&temporary_path, path).is_err() {
        return Err(cleanup_after_failure(&temporary_path, AliasError::Replace));
    }

    Ok(sync_directory(parent))
}

fn cleanup_after_failure(path: &Path, original: AliasError) -> AliasError {
    if fs::remove_file(path).is_err() {
        tracing::warn!(
            event = "alias_temporary_cleanup_failed",
            "Alias temporary cleanup failed"
        );
    }
    original
}

fn sync_directory(_path: &Path) -> bool {
    #[cfg(unix)]
    {
        OpenOptions::new()
            .read(true)
            .open(_path)
            .and_then(|directory| directory.sync_all())
            .is_ok()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        AddResult, Alias, AliasError, AliasFile, AliasStore, DeleteResult, MAX_FILE_BYTES,
        normalize_name, serialize_aliases, validate_alias, validate_aliases, validate_args,
    };

    fn test_path(label: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "lavis-alias-{label}-{}-{nonce}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory.join("aliases.json")
    }

    fn alias() -> Alias {
        Alias {
            target: "ping".to_owned(),
            args: vec!["--plain".to_owned()],
        }
    }

    #[test]
    fn normalizes_names_and_rejects_reserved_or_invalid_targets() {
        assert_eq!(normalize_name("FAST-Info").unwrap(), "fast-info");
        assert_eq!(normalize_name("1fast"), Err(AliasError::InvalidName));
        assert_eq!(
            validate_alias("ping", &alias()),
            Err(AliasError::ReservedName)
        );
        assert_eq!(
            validate_alias("prefix", &alias()),
            Err(AliasError::PrefixReserved)
        );
        assert_eq!(
            validate_alias("modules", &alias()),
            Err(AliasError::ModulesReserved)
        );
        assert_eq!(
            validate_alias(
                "fast",
                &Alias {
                    target: "alias".to_owned(),
                    args: Vec::new(),
                }
            ),
            Err(AliasError::TargetNotAliasable)
        );
        assert_eq!(
            validate_alias(
                "again",
                &Alias {
                    target: "reboot".to_owned(),
                    args: Vec::new(),
                }
            ),
            Err(AliasError::TargetNotAliasable)
        );
        assert_eq!(
            validate_alias(
                "fast",
                &Alias {
                    target: "missing".to_owned(),
                    args: Vec::new(),
                }
            ),
            Err(AliasError::UnknownTarget)
        );
    }

    #[test]
    fn validates_argument_bounds() {
        assert_eq!(
            validate_args(&["x".repeat(257)]),
            Err(AliasError::ArgumentTooLong)
        );
        assert_eq!(
            validate_args(&vec!["x".to_owned(); 33]),
            Err(AliasError::ArgumentLimit)
        );
        assert_eq!(
            validate_args(&vec!["x".repeat(256); 9]),
            Err(AliasError::ArgumentsTooLarge)
        );
    }

    #[test]
    fn validates_alias_limit() {
        let aliases = (0..129)
            .map(|index| (format!("a{index}"), alias()))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(validate_aliases(&aliases), Err(AliasError::AliasLimit));
    }

    #[test]
    fn serializes_aliases_deterministically() {
        let mut aliases = BTreeMap::new();
        aliases.insert("zeta".to_owned(), alias());
        aliases.insert("alpha".to_owned(), alias());

        let json = String::from_utf8(serialize_aliases(&aliases).unwrap()).unwrap();
        assert!(json.find("alpha").unwrap() < json.find("zeta").unwrap());
        assert_eq!(parse_aliases_for_test(&json).unwrap(), aliases);
    }

    #[tokio::test]
    async fn loads_missing_and_rejects_invalid_files() {
        let path = test_path("load");
        let store = AliasStore::load(path.clone()).await.unwrap();
        assert!(store.aliases().is_empty());

        fs::write(&path, "not json").unwrap();
        assert!(matches!(
            AliasStore::load(path.clone()).await,
            Err(AliasError::MalformedFile)
        ));
        fs::write(&path, r#"{"version":2,"aliases":{}}"#).unwrap();
        assert!(matches!(
            AliasStore::load(path.clone()).await,
            Err(AliasError::UnsupportedVersion)
        ));
        fs::write(
            &path,
            r#"{"version":1,"aliases":{"ping":{"target":"ping","args":[]}}}"#,
        )
        .unwrap();
        assert!(matches!(
            AliasStore::load(path.clone()).await,
            Err(AliasError::ReservedName)
        ));
        fs::write(&path, vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
        assert!(matches!(
            AliasStore::load(path.clone()).await,
            Err(AliasError::FileTooLarge)
        ));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn legacy_reserved_alias_fixtures_are_rejected_without_rewriting_them() {
        let path = test_path("legacy-reserved");
        for (bytes, expected) in [
            (
                br#"{"version":1,"aliases":{"prefix":{"target":"ping","args":[]}}}"#.as_slice(),
                AliasError::PrefixReserved,
            ),
            (
                br#"{"version":1,"aliases":{"modules":{"target":"ping","args":[]}}}"#.as_slice(),
                AliasError::ModulesReserved,
            ),
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(matches!(
                AliasStore::load(path.clone()).await,
                Err(error) if error == expected
            ));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn persists_add_delete_and_preserves_memory_on_validation_failure() {
        let path = test_path("mutate");
        let mut store = AliasStore::load(path.clone()).await.unwrap();

        assert_eq!(store.add("Fast", alias()).await.unwrap(), AddResult::Added);
        assert_eq!(store.lookup("FAST"), Some(&alias()));
        let invocation = store
            .invocation("fast", &["--extra".to_owned()])
            .unwrap()
            .unwrap();
        assert_eq!(invocation.target, "ping");
        assert_eq!(invocation.args, ["--plain", "--extra"]);
        assert_eq!(
            store.invocation("fast", &vec!["x".repeat(256); 8]),
            Err(AliasError::ArgumentsTooLarge)
        );
        assert_eq!(
            AliasStore::load(path.clone()).await.unwrap().aliases(),
            store.aliases()
        );

        assert_eq!(
            store.add("bad name", alias()).await,
            Err(AliasError::InvalidName)
        );
        assert!(store.lookup("fast").is_some());
        let aliases_before_duplicate = store.aliases().clone();
        let file_before_duplicate = fs::read(&path).unwrap();
        assert_eq!(
            store.add("FAST", alias()).await,
            Err(AliasError::AlreadyExists)
        );
        assert_eq!(store.aliases(), &aliases_before_duplicate);
        assert_eq!(fs::read(&path).unwrap(), file_before_duplicate);

        let mut oversized_candidate = store.aliases().clone();
        for index in 0..8 {
            oversized_candidate.insert(
                format!("large{index}"),
                Alias {
                    target: "ping".to_owned(),
                    args: vec!["x".repeat(256); 32],
                },
            );
        }
        assert_eq!(
            store.commit(oversized_candidate).await,
            Err(AliasError::FileTooLarge)
        );
        assert_eq!(store.aliases(), &aliases_before_duplicate);
        assert_eq!(fs::read(&path).unwrap(), file_before_duplicate);
        assert_eq!(store.delete("fast").await.unwrap(), DeleteResult::Deleted);
        assert_eq!(store.delete("fast").await.unwrap(), DeleteResult::NotFound);
        assert!(
            AliasStore::load(path.clone())
                .await
                .unwrap()
                .aliases()
                .is_empty()
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn refuses_to_create_a_missing_state_directory() {
        let path = test_path("missing-parent").with_file_name("missing/aliases.json");
        let mut store = AliasStore::load(path).await.unwrap();

        assert_eq!(
            store.add("fast", alias()).await,
            Err(AliasError::CreateDirectory)
        );
        assert!(store.aliases().is_empty());
        fs::remove_dir_all(store.path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persists_alias_files_with_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = test_path("permissions");
        let mut store = AliasStore::load(path.clone()).await.unwrap();
        store.add("fast", alias()).await.unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn parse_aliases_for_test(json: &str) -> Result<BTreeMap<String, Alias>, AliasError> {
        let alias_file: AliasFile =
            serde_json::from_str(json).map_err(|_| AliasError::MalformedFile)?;
        if alias_file.version != super::VERSION {
            return Err(AliasError::UnsupportedVersion);
        }
        super::validate_aliases(&alias_file.aliases)?;
        Ok(alias_file.aliases)
    }
}
