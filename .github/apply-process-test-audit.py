from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one match in {path}, got {count}")
    file.write_text(text.replace(old, new, 1))


replace_once(
    "src/external_modules/process.rs",
    '''    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const ECHO_MODULE_PY:''',
    '''    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn test_nonce() -> String {
        format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        )
    }

    const ECHO_MODULE_PY:''',
)

replace_once(
    "src/external_modules/process.rs",
    '''    fn create_echo_module() -> (ExternalModuleDescriptor, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("lavis-proc-test-{nonce}"));''',
    '''    fn create_echo_module() -> (ExternalModuleDescriptor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lavis-proc-test-{}", test_nonce()));''',
)

replace_once(
    "src/external_modules/process.rs",
    '''    fn create_child_spawner_module() -> (ExternalModuleDescriptor, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("lavis-proc-child-{nonce}"));''',
    '''    fn create_child_spawner_module() -> (ExternalModuleDescriptor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lavis-proc-child-{}", test_nonce()));''',
)

replace_once(
    "src/external_modules/process.rs",
    '''    fn create_fixture_module(body: &str, id: &str) -> (ExternalModuleDescriptor, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("lavis-fixture-{nonce}"));''',
    '''    fn create_fixture_module(body: &str, id: &str) -> (ExternalModuleDescriptor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lavis-fixture-{}", test_nonce()));''',
)
