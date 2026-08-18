//! Integration tests for the `lavis-v6-conformance` runner binary.
//!
//! The runner drives a real child process through the documented v6 lifecycle
//! transcript (initialize, execute, event, health, shutdown) and validates
//! every frame with the production parser. These tests pin the transcript
//! order, the base/full profile split (a module without `raw.invoke` must pass
//! base conformance), and the bounded shutdown wait.
//!
//! The fixture modules are spawned through an explicit `python3` interpreter
//! rather than their `#!/usr/bin/env python3` shebang: the Nix build sandbox
//! has no `/usr/bin/env`, so shebang-based spawns fail there. `python3` is on
//! PATH in both the dev shell and the sandbox (flake `nativeBuildInputs`).

use std::{fs, os::unix::fs::PermissionsExt, path::Path, path::PathBuf, time::Duration};

use tokio::process::Command;

const RUNNER: &str = env!("CARGO_BIN_EXE_lavis-v6-conformance");

/// Follows the documented transcript strictly: asserts the exact frame order
/// and makes no Telegram calls at all.
const CURATED_ONLY_SCRIPT: &str = r#"#!/usr/bin/env python3
import sys, json

expected = ["initialize", "execute", "event", "health", "shutdown"]
for want in expected:
    line = sys.stdin.readline()
    if not line:
        sys.exit(80)
    frame = json.loads(line)
    if frame["type"] != want:
        sys.exit(81)
    rid = frame["request_id"]
    if want == "initialize":
        print(json.dumps({"protocol_version": 6, "type": "initialized", "request_id": rid, "module_id": "conformance"}), flush=True)
    elif want == "execute":
        print(json.dumps({"protocol_version": 6, "type": "result", "request_id": rid, "text": "ok"}), flush=True)
    elif want == "event":
        print(json.dumps({"protocol_version": 6, "type": "event_result", "request_id": rid, "actions": []}), flush=True)
    elif want == "health":
        print(json.dumps({"protocol_version": 6, "type": "health", "request_id": rid}), flush=True)
sys.exit(0)
"#;

/// Same strict transcript, but also exercises one curated helper and one
/// `raw.invoke` call after the event response. The module ignores
/// `telegram.result` completions (correlated by call_id, not by order) and
/// waits for the expected lifecycle frame, mirroring real module behavior.
const FULL_SCRIPT: &str = r#"#!/usr/bin/env python3
import sys, json

expected = ["initialize", "execute", "event", "health", "shutdown"]
for want in expected:
    while True:
        line = sys.stdin.readline()
        if not line:
            sys.exit(80)
        frame = json.loads(line)
        if frame["type"] == "telegram.result":
            continue
        if frame["type"] != want:
            sys.exit(81)
        break
    rid = frame["request_id"]
    if want == "initialize":
        print(json.dumps({"protocol_version": 6, "type": "initialized", "request_id": rid, "module_id": "conformance"}), flush=True)
    elif want == "execute":
        print(json.dumps({"protocol_version": 6, "type": "result", "request_id": rid, "text": "ok"}), flush=True)
    elif want == "event":
        print(json.dumps({"protocol_version": 6, "type": "event_result", "request_id": rid, "actions": []}), flush=True)
        print('{"protocol_version":6,"type":"telegram.invoke","call_id":"curated-1","method":"contacts.getContacts","params":{"hash":"0"}}', flush=True)
        print('{"protocol_version":6,"type":"telegram.invoke","call_id":"raw-1","method":"raw.invoke","params":{"body_base64_chunks":["eFY0EgEAAAA="]}}', flush=True)
    elif want == "health":
        print(json.dumps({"protocol_version": 6, "type": "health", "request_id": rid}), flush=True)
sys.exit(0)
"#;

/// Completes the lifecycle but never exits after the shutdown frame.
const HANG_ON_SHUTDOWN_SCRIPT: &str = r#"#!/usr/bin/env python3
import sys, json, time

expected = ["initialize", "execute", "event", "health", "shutdown"]
for want in expected:
    line = sys.stdin.readline()
    if not line:
        sys.exit(80)
    frame = json.loads(line)
    if frame["type"] != want:
        sys.exit(81)
    rid = frame["request_id"]
    if want == "initialize":
        print(json.dumps({"protocol_version": 6, "type": "initialized", "request_id": rid, "module_id": "conformance"}), flush=True)
    elif want == "execute":
        print(json.dumps({"protocol_version": 6, "type": "result", "request_id": rid, "text": "ok"}), flush=True)
    elif want == "event":
        print(json.dumps({"protocol_version": 6, "type": "event_result", "request_id": rid, "actions": []}), flush=True)
    elif want == "health":
        print(json.dumps({"protocol_version": 6, "type": "health", "request_id": rid}), flush=True)
time.sleep(3600)
"#;

fn fixture_dir(label: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "lavis-v6-conformance-{label}-{}-{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn write_module(directory: &Path, body: &str) -> PathBuf {
    let entrypoint = directory.join("run");
    fs::write(&entrypoint, body).unwrap();
    fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o700)).unwrap();
    entrypoint
}

async fn run_runner(arguments: &[&str], deadline: Duration) -> (bool, String) {
    let output = tokio::time::timeout(deadline, Command::new(RUNNER).args(arguments).output())
        .await
        .expect("conformance runner hung beyond the test deadline");
    let output = output.expect("failed to run the conformance runner");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.success(), format!("{stdout}\n{stderr}"))
}

#[tokio::test]
async fn base_profile_passes_a_curated_only_module_in_documented_order() {
    let directory = fixture_dir("curated");
    let module = write_module(&directory, CURATED_ONLY_SCRIPT);
    let (ok, output) = run_runner(
        &["python3", module.to_str().unwrap()],
        Duration::from_secs(30),
    )
    .await;
    assert!(ok, "base profile rejected a curated-only module: {output}");
    assert!(output.contains("v6 alpha conformance: passed"));
    let _ = fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn full_profile_requires_raw_invoke() {
    let directory = fixture_dir("curated");
    let module = write_module(&directory, CURATED_ONLY_SCRIPT);
    let (ok, output) = run_runner(
        &["--profile", "full", "python3", module.to_str().unwrap()],
        Duration::from_secs(30),
    )
    .await;
    assert!(
        !ok,
        "full profile accepted a module without raw.invoke: {output}"
    );
    assert!(
        output.contains("full RPC capability profile requires curated and raw.invoke calls"),
        "unexpected failure output: {output}"
    );
    let _ = fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn full_profile_passes_a_module_with_curated_and_raw_calls() {
    let directory = fixture_dir("full");
    let module = write_module(&directory, FULL_SCRIPT);
    let (ok, output) = run_runner(
        &["--profile", "full", "python3", module.to_str().unwrap()],
        Duration::from_secs(30),
    )
    .await;
    assert!(
        ok,
        "full profile rejected a module with curated and raw calls: {output}"
    );
    assert!(output.contains("v6 alpha conformance: passed"));
    let _ = fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn shutdown_wait_is_bounded_for_a_hanging_module() {
    let directory = fixture_dir("hang");
    let module = write_module(&directory, HANG_ON_SHUTDOWN_SCRIPT);
    let (ok, output) = run_runner(
        &["python3", module.to_str().unwrap()],
        Duration::from_secs(30),
    )
    .await;
    assert!(!ok, "hanging module unexpectedly passed: {output}");
    assert!(
        output.contains("did not exit within the shutdown deadline"),
        "unexpected failure output: {output}"
    );
    let _ = fs::remove_dir_all(&directory);
}
