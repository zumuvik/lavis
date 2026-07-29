from pathlib import Path

path = Path("src/credentials.rs")
text = path.read_text()
old_import = '''    use std::{
        collections::HashMap,
        time::{SystemTime, UNIX_EPOCH},
    };
'''
new_import = '''    use std::{
        collections::HashMap,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
'''
if text.count(old_import) != 1:
    raise SystemExit(f"expected one credentials test import, got {text.count(old_import)}")
text = text.replace(old_import, new_import, 1)
old_path = '''    fn path() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "lavis-credentials-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
'''
new_path = '''    fn path() -> PathBuf {
        static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "lavis-credentials-{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
'''
if text.count(old_path) != 1:
    raise SystemExit(f"expected one credentials test path helper, got {text.count(old_path)}")
path.write_text(text.replace(old_path, new_path, 1))
