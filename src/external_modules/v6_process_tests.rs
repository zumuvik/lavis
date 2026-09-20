use super::*;
use std::path::PathBuf;

use crate::external_modules::{
    manifest,
    source_inspection::{
        AcquiredLmod, InspectionConfig, InspectionLimits, ModuleInspector, OsRandom,
    },
    v6_executor::{self, V6ExecutorError},
};

#[test]
fn v6_alpha_race_and_timeout_contract_matches_process_boundaries() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../protocol/v6/alpha-contract.json")).unwrap();
    assert_eq!(fixture["timeout_start"], "after_write_flush");
    assert_eq!(lifecycle_timeout(), V6_LIFECYCLE_TIMEOUT);
    assert_eq!(V6_RPC_TIMEOUT, Duration::from_secs(5));
    assert_eq!(
        fixture["race_semantics"]["duplicate_active_call_id"],
        "protocol_violation"
    );
    assert_eq!(
        fixture["race_semantics"]["cancellation"],
        "drain_active_lifecycle_log"
    );
    assert_eq!(
        fixture["race_semantics"]["shutdown"],
        "discard_late_telegram_result"
    );
}

fn descriptor() -> ExternalModuleDescriptor {
    ExternalModuleDescriptor {
        protocol_version: 6,
        contract_revision: Some(2),
        id: "test".to_owned(),
        display_name: "Test".to_owned(),
        version: "1".to_owned(),
        author: "A".to_owned(),
        entrypoint: PathBuf::from("run"),
        module_dir: PathBuf::from("."),
        capabilities: vec![],
        default_command: None,
        subscriptions: vec![],
        telegram_methods: vec![],
        actions: vec![],
        commands: vec![],
    }
}

#[derive(Default)]
struct RecordingExecutor {
    methods: std::sync::Mutex<Vec<v6_registry::V6Method>>,
}

impl V6TelegramExecutor for RecordingExecutor {
    fn execute<'a>(
        &'a self,
        _context: V6ExecutionContext,
        method: v6_registry::V6Method,
        _params: Box<serde_json::value::RawValue>,
    ) -> super::super::v6_executor::V6ExecutorFuture<'a> {
        self.methods.lock().unwrap().push(method);
        Box::pin(async {
            super::super::v6_executor::V6RpcOutput::new(serde_json::json!({"fixture": true}))
        })
    }
}

fn test_root(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "lavis-{prefix}-{}-{}",
        std::process::id(),
        protocol::request_id()
    ))
}

/// Absolute path to a `python3` interpreter found via the test process
/// PATH, mirroring the legacy process fixtures. The child environment is
/// cleared (`PATH=/usr/bin:/bin`), so fixture scripts must carry an
/// absolute interpreter path that exists in the sandbox.
fn python_executable() -> PathBuf {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join("python3"))
        .find(|candidate| candidate.is_file())
        .expect("fixture tests require python3 in PATH")
}

/// Write a v6 fixture script whose `#!/usr/bin/env python3` shebang is
/// rewritten to the absolute interpreter path, so the cleared child
/// environment can still execute it.
#[cfg(unix)]
fn write_v6_fixture(path: &Path, body: &str) {
    use std::{fs, os::unix::fs::PermissionsExt};
    let body = body.replacen(
        "#!/usr/bin/env python3",
        &format!("#!{}", python_executable().display()),
        1,
    );
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

/// In-memory variant of `write_v6_fixture` for archive payloads.
#[cfg(unix)]
fn fixture_script(body: &str) -> Vec<u8> {
    body.replacen(
        "#!/usr/bin/env python3",
        &format!("#!{}", python_executable().display()),
        1,
    )
    .into_bytes()
}

// Minimal STORED-only zip writer for packaged-module fixtures. Mirrors the
// source_inspection test helper; no compression is used, so the archive is
// inspectable by the production `inspect_into` path.
const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const EOCD: u32 = 0x0605_4b50;

struct ArchiveEntry {
    name: String,
    data: Vec<u8>,
    mode: u32,
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn zip_entries(entries: &[ArchiveEntry]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut offsets = Vec::new();
    for entry in entries {
        offsets.push(output.len() as u32);
        put32(&mut output, LOCAL);
        put16(&mut output, 20);
        put16(&mut output, 0);
        put16(&mut output, 0);
        put32(&mut output, 0);
        put32(&mut output, 0);
        put32(&mut output, entry.data.len() as u32);
        put32(&mut output, entry.data.len() as u32);
        put16(&mut output, entry.name.len() as u16);
        put16(&mut output, 0);
        output.extend_from_slice(entry.name.as_bytes());
        output.extend_from_slice(&entry.data);
    }
    let central_start = output.len() as u32;
    for (entry, offset) in entries.iter().zip(offsets) {
        put32(&mut output, CENTRAL);
        put16(&mut output, 0x0314);
        put16(&mut output, 20);
        put16(&mut output, 0);
        put16(&mut output, 0);
        put32(&mut output, 0);
        put32(&mut output, 0);
        put32(&mut output, entry.data.len() as u32);
        put32(&mut output, entry.data.len() as u32);
        put16(&mut output, entry.name.len() as u16);
        put16(&mut output, 0);
        put16(&mut output, 0);
        put16(&mut output, 0);
        put16(&mut output, 0);
        put32(&mut output, entry.mode << 16);
        put32(&mut output, offset);
        output.extend_from_slice(entry.name.as_bytes());
    }
    let central_length = output.len() as u32 - central_start;
    put32(&mut output, EOCD);
    put16(&mut output, 0);
    put16(&mut output, 0);
    put16(&mut output, entries.len() as u16);
    put16(&mut output, entries.len() as u16);
    put32(&mut output, central_length);
    put32(&mut output, central_start);
    put16(&mut output, 0);
    output
}

/// Opaque bodies received by a fake transport.
type ReceivedCalls = Arc<std::sync::Mutex<Vec<(i32, Vec<u8>)>>>;

/// Fake transport recording the opaque bodies it receives and answering
/// with opaque bytes, so the packaged module's `raw.invoke` can be verified
/// end-to-end without Telegram.
struct FakeRawTlTransport {
    home_dc: Option<i32>,
    response: Vec<u8>,
    received: ReceivedCalls,
}

impl v6_executor::RawTlTransport for FakeRawTlTransport {
    fn home_dc_id(&self) -> Option<i32> {
        self.home_dc
    }

    fn invoke_in_dc(
        &self,
        dc_id: i32,
        body: Vec<u8>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<u8>, V6ExecutorError>> + Send + '_>,
    > {
        Box::pin(async move {
            self.received.lock().unwrap().push((dc_id, body));
            Ok(self.response.clone())
        })
    }
}

/// Executor routing `raw.invoke` through the transport-generic pipeline.
/// Curated RPCs observed by the packaged-module executor.
type CuratedCalls = std::sync::Mutex<Vec<(v6_registry::V6Method, serde_json::Value)>>;

/// Test executor for the packaged `.lmod` fixture: serves one curated
/// helper through the normal executor contract and routes `raw.invoke`
/// through the injectable fake transport.
struct PackagedExecutor {
    transport: Arc<dyn v6_executor::RawTlTransport>,
    curated: Arc<CuratedCalls>,
}

impl V6TelegramExecutor for PackagedExecutor {
    fn execute<'a>(
        &'a self,
        _context: V6ExecutionContext,
        method: v6_registry::V6Method,
        params: Box<serde_json::value::RawValue>,
    ) -> super::super::v6_executor::V6ExecutorFuture<'a> {
        let transport = self.transport.clone();
        Box::pin(async move {
            match method {
                v6_registry::V6Method::MessagesGetDialogs => {
                    let value: serde_json::Value = serde_json::from_str(params.get())
                        .map_err(|_| V6ExecutorError::InvalidParams("malformed params"))?;
                    // Mirror the production contract: the module's curated
                    // call must carry a bounded page limit and a hash.
                    if value.get("limit").and_then(serde_json::Value::as_i64) != Some(10) {
                        return Err(V6ExecutorError::InvalidParams("limit"));
                    }
                    if value.get("hash").and_then(serde_json::Value::as_str) != Some("0") {
                        return Err(V6ExecutorError::InvalidParams("hash"));
                    }
                    self.curated.lock().unwrap().push((method, value.clone()));
                    v6_executor::V6RpcOutput::new(serde_json::json!({
                        "kind": "dialogs_summary",
                        "dialogs_count": 2,
                        "messages_count": 1,
                        "chats_count": 1,
                        "users_count": 1,
                        "truncated": false,
                    }))
                }
                v6_registry::V6Method::RawInvoke => {
                    v6_executor::raw_invoke_pipeline(transport.as_ref(), params).await
                }
                _ => Err(V6ExecutorError::InvalidParams("unexpected method")),
            }
        })
    }
}

#[test]
fn writer_queue_distinguishes_backpressure_from_closed_writer() {
    let (writer, mut receiver) = mpsc::channel(1);
    assert!(
        queue_writer(
            &writer,
            WriterCommand::Frame(
                V6OutboundCoreFrame::Health {
                    request_id: "1".to_owned()
                },
                Flush::None,
            ),
        )
        .is_ok()
    );
    assert_eq!(
        queue_writer(
            &writer,
            WriterCommand::Frame(
                V6OutboundCoreFrame::Health {
                    request_id: "2".to_owned()
                },
                Flush::None,
            ),
        ),
        Err(WriterQueueFailure::Backpressure),
    );
    assert!(receiver.try_recv().is_ok());
    drop(receiver);
    assert_eq!(
        queue_writer(
            &writer,
            WriterCommand::Frame(
                V6OutboundCoreFrame::Health {
                    request_id: "3".to_owned()
                },
                Flush::None,
            ),
        ),
        Err(WriterQueueFailure::Closed),
    );
}

#[test]
fn rpc_complete_before_shutdown_is_written() {
    let (writer, mut receiver) = mpsc::channel(1);
    let mut active_calls = HashSet::from(["call-1".to_owned()]);
    let disposition = handle_rpc_complete(
        false,
        &mut active_calls,
        &writer,
        "call-1".to_owned(),
        Ok(serde_json::json!({"fixture": true})),
    )
    .unwrap();
    assert_eq!(disposition, RpcCompleteDisposition::Written);
    assert!(active_calls.contains("call-1"));
    match receiver.try_recv().unwrap() {
        WriterCommand::Frame(
            V6OutboundCoreFrame::TelegramResult { call_id, .. },
            Flush::Call(flush_id),
        ) => {
            assert_eq!(call_id, "call-1");
            assert_eq!(flush_id, "call-1");
        }
        other => panic!("unexpected writer command: {other:?}"),
    }
}

#[test]
fn rpc_complete_after_shutdown_is_discarded_without_write() {
    let (writer, mut receiver) = mpsc::channel(1);
    let mut active_calls = HashSet::from(["call-2".to_owned()]);
    let disposition = handle_rpc_complete(
        true,
        &mut active_calls,
        &writer,
        "call-2".to_owned(),
        Ok(serde_json::json!({"fixture": true})),
    )
    .unwrap();
    assert_eq!(disposition, RpcCompleteDisposition::Discarded);
    assert!(!active_calls.contains("call-2"));
    assert!(receiver.try_recv().is_err());
}

#[test]
fn rpc_complete_releases_bookkeeping_for_unknown_call() {
    let (writer, mut receiver) = mpsc::channel(1);
    let mut active_calls = HashSet::from(["call-3".to_owned()]);
    let disposition = handle_rpc_complete(
        true,
        &mut active_calls,
        &writer,
        "unknown".to_owned(),
        Ok(serde_json::json!({})),
    )
    .unwrap();
    assert_eq!(disposition, RpcCompleteDisposition::Discarded);
    assert!(active_calls.contains("call-3"));
    assert!(receiver.try_recv().is_err());
}

#[test]
fn rpc_complete_writer_backpressure_is_fatal() {
    let (writer, _receiver) = mpsc::channel(1);
    // Fill the queue so the completion hits backpressure.
    writer
        .try_send(WriterCommand::Frame(
            V6OutboundCoreFrame::Health {
                request_id: "fill".to_owned(),
            },
            Flush::None,
        ))
        .unwrap();
    let mut active_calls = HashSet::new();
    let error = handle_rpc_complete(
        false,
        &mut active_calls,
        &writer,
        "call-4".to_owned(),
        Ok(serde_json::json!({})),
    )
    .unwrap_err();
    assert!(matches!(error, FatalReason::Backpressure));
}

/// Executor that records when the RPC future starts and only completes
/// after a delay, so the completion reliably lands after shutdown begins.
#[derive(Default)]
struct DelayedExecutor {
    started: Arc<std::sync::atomic::AtomicBool>,
}

impl V6TelegramExecutor for DelayedExecutor {
    fn execute<'a>(
        &'a self,
        _context: V6ExecutionContext,
        _method: v6_registry::V6Method,
        _params: Box<serde_json::value::RawValue>,
    ) -> super::super::v6_executor::V6ExecutorFuture<'a> {
        let started = self.started.clone();
        Box::pin(async move {
            started.store(true, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await;
            super::super::v6_executor::V6RpcOutput::new(serde_json::json!({"fixture": true}))
        })
    }
}

/// Regression: an RPC completing after shutdown began must not enqueue a
/// `telegram.result` behind the `shutdown` frame. The child exits 5 if it
/// ever observes a `telegram.result`; with the barrier it only receives the
/// shutdown frame, exits 0, and shutdown completes cleanly.
#[cfg(unix)]
#[tokio::test]
async fn rpc_completing_during_shutdown_is_discarded_and_shutdown_stays_clean() {
    use std::fs;
    let root = test_root("v6-rpc-shutdown-barrier");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let entrypoint = root.join("run");
    ensure_test_state_base();

    let mut module = descriptor();
    module.id = format!("v6rpcshutdown{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = entrypoint.clone();
    module.telegram_methods = vec![v6_registry::V6Method::ContactsGetContacts];

    let script = r#"#!/usr/bin/env python3
import sys, json

def req_id(line):
    return json.loads(line)["request_id"]

line = sys.stdin.readline()
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "initialized", "request_id": rid, "module_id": "__MODULE_ID__"}), flush=True)
line = sys.stdin.readline()
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "health", "request_id": rid}), flush=True)
print('{"protocol_version":6,"type":"telegram.invoke","call_id":"race-1","method":"contacts.getContacts","params":{"hash":"0"}}', flush=True)
line = sys.stdin.readline()
if line and '"telegram.result"' in line:
    print('telegram.result delivered after shutdown', file=sys.stderr)
    sys.stderr.flush()
    sys.exit(5)
sys.exit(0)
"#
        .replace("__MODULE_ID__", &module.id);
    write_v6_fixture(&entrypoint, &script);

    let executor = Arc::new(DelayedExecutor::default());
    let v6proc = V6Process::start(module.clone(), executor.clone(), 1)
        .await
        .unwrap();
    v6proc.initialize("1".to_owned(), module.id).await.unwrap();
    v6proc.health("2".to_owned()).await.unwrap();

    // Wait until the RPC is in flight, then begin shutdown so the
    // completion lands while the supervisor is closing.
    for _ in 0..100 {
        if executor.started.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(executor.started.load(std::sync::atomic::Ordering::SeqCst));

    assert!(v6proc.graceful_shutdown().await.is_ok());
    assert_eq!(v6proc.status(), process::ProcessStatus::Terminated);
    assert!(v6proc.diagnostic().is_none());
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
async fn late_cancelled_lifecycle_frame_is_drained(kind: &str) {
    use std::fs;

    let root = test_root(&format!("v6-late-cancelled-{kind}"));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let entrypoint = root.join("run");
    ensure_test_state_base();

    let mut module = descriptor();
    // State directories are keyed by module ID.  A per-fixture ID avoids
    // concurrent test instances sharing state when the test binary or its
    // temporary directory is reused by a sandbox runner.
    module.id = format!(
        "v6late{kind}-{}-{}",
        std::process::id(),
        protocol::request_id()
    );
    module.module_dir = root.clone();
    module.entrypoint = entrypoint.clone();
    let late_expression = match kind {
        "result" => r#"{'protocol_version':6,'type':'result','request_id':rid,'text':'late'}"#,
        "error" => {
            r#"{'protocol_version':6,'type':'error','request_id':rid,'code':'late','message':'late'}"#
        }
        "event_result" => {
            r#"{'protocol_version':6,'type':'event_result','request_id':rid,'actions':[]}"#
        }
        "health" => r#"{'protocol_version':6,'type':'health','request_id':rid}"#,
        "log" => {
            r#"{'protocol_version':6,'type':'log','request_id':rid,'level':'info','message':'late'}"#
        }
        _ => panic!("unsupported late frame kind"),
    };
    let script = r#"#!/usr/bin/env python3
import json, sys

line = sys.stdin.readline()
rid = json.loads(line)["request_id"]
print(json.dumps({"protocol_version":6,"type":"initialized","request_id":rid,"module_id":"__MODULE_ID__"}), flush=True)
line = sys.stdin.readline()
frame = json.loads(line)
assert frame["type"] == "execute"
rid = frame["request_id"]
open("execute-received", "w").close()
# Wait for the shutdown barrier, then emit a response to the cancelled request.
line = sys.stdin.readline()
assert json.loads(line)["type"] == "shutdown"
print(json.dumps(__LATE_FRAME__), flush=True)
sys.exit(0)
"#
            .replace("__MODULE_ID__", &module.id)
            .replace("__LATE_FRAME__", late_expression);
    write_v6_fixture(&entrypoint, &script);

    let process = V6Process::start(module.clone(), Arc::new(RecordingExecutor::default()), 1)
        .await
        .unwrap();
    process.initialize("1".to_owned(), module.id).await.unwrap();
    let execute = {
        let process = process.clone();
        tokio::spawn(async move {
            process
                .execute("2".to_owned(), "go".to_owned(), String::new(), vec![])
                .await
        })
    };
    for _ in 0..100 {
        if root.join("execute-received").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(root.join("execute-received").exists());
    process.graceful_shutdown().await.unwrap();
    assert!(matches!(
        execute.await.unwrap(),
        Err(ExternalError::Unavailable)
    ));
    assert_eq!(process.status(), process::ProcessStatus::Terminated);
    assert!(process.diagnostic().is_none());
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn late_cancelled_lifecycle_result_does_not_crash_shutdown() {
    late_cancelled_lifecycle_frame_is_drained("result").await;
}

#[cfg(unix)]
#[tokio::test]
async fn late_cancelled_lifecycle_log_does_not_crash_shutdown() {
    late_cancelled_lifecycle_frame_is_drained("log").await;
}

#[cfg(unix)]
#[tokio::test]
async fn late_cancelled_lifecycle_terminal_variants_do_not_crash_shutdown() {
    for kind in ["error", "event_result", "health"] {
        late_cancelled_lifecycle_frame_is_drained(kind).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn post_shutdown_telegram_invoke_does_not_cross_barrier() {
    use std::fs;

    let root = test_root("v6-post-shutdown-invoke");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let entrypoint = root.join("run");
    ensure_test_state_base();
    let mut module = descriptor();
    module.id = format!("v6postinvoke{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = entrypoint.clone();
    module.telegram_methods = vec![v6_registry::V6Method::ContactsGetContacts];
    write_v6_fixture(
        &entrypoint,
        &format!(
            r#"#!/usr/bin/env python3
import json, os, select, sys, time
line = sys.stdin.readline()
rid = json.loads(line)["request_id"]
print(json.dumps({{"protocol_version":6,"type":"initialized","request_id":rid,"module_id":"{}"}}), flush=True)
line = sys.stdin.readline()
assert json.loads(line)["type"] == "shutdown"
print('{{"protocol_version":6,"type":"telegram.invoke","call_id":"after-shutdown","method":"contacts.getContacts","params":{{"hash":"0"}}}}', flush=True)
# The shutdown frame has already crossed the writer barrier. Keep the child
# alive for a bounded nonblocking/select window and reject any host-directed
# frame, including a trailing telegram.result.
fd = sys.stdin.fileno()
os.set_blocking(fd, False)
deadline = time.monotonic() + 0.2
while time.monotonic() < deadline:
    ready, _, _ = select.select([fd], [], [], min(0.02, deadline - time.monotonic()))
    if not ready:
        continue
    try:
        data = os.read(fd, 65536)
    except BlockingIOError:
        continue
    if data:
        print("host-directed frame observed", file=sys.stderr)
        sys.exit(9)
    break
sys.exit(0)
"#,
            module.id
        ),
    );
    let executor = Arc::new(RecordingExecutor::default());
    let process = V6Process::start(module.clone(), executor.clone(), 1)
        .await
        .unwrap();
    process.initialize("1".to_owned(), module.id).await.unwrap();
    process.graceful_shutdown().await.unwrap();
    assert_eq!(process.status(), process::ProcessStatus::Terminated);
    assert!(process.diagnostic().is_none());
    assert!(executor.methods.lock().unwrap().is_empty());
    let _ = fs::remove_dir_all(&root);
}

/// Packaged `.lmod` conformance: the archive is inspected and validated
/// through the production pipeline (zip inspection + manifest validation),
/// then the v6 child is started and drives `raw.invoke` with opaque TL
/// bytes for `messages.getPinnedDialogs` — a valid method with no typed registry
/// entry — through a fake transport and back into the module.
#[cfg(unix)]
#[tokio::test]
async fn packaged_lmod_conforms_through_full_lifecycle_with_curated_and_raw_rpc() {
    use grammers_client::tl::Serializable;
    use std::fs;

    let root = test_root("v6-packaged");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let module_id = format!("packagedv6{}", std::process::id());

    // Module-owned opaque raw request: a fully serialized, read-only
    // Telegram function that Lavis has no typed adapter for:
    //   messages.getPinnedDialogs, folder_id = 1
    //   TL layer 227, constructor 0xd6b94df2
    // Layout: LE constructor id (f2 4d b9 d6), then LE int32 folder_id.
    const RAW_BODY: [u8; 8] = [0xf2, 0x4d, 0xb9, 0xd6, 0x01, 0x00, 0x00, 0x00];
    assert!(
        v6_registry::lookup("messages.getPinnedDialogs").is_none(),
        "raw fixture method must not be in the typed registry"
    );
    // Independently prove validity: serialize the same function with the
    // generated TL definitions used by the locked grammers dependency and
    // require the module's hard-coded bytes to match exactly.
    let reference =
        grammers_client::tl::functions::messages::GetPinnedDialogs { folder_id: 1 }.to_bytes();
    assert_eq!(
        RAW_BODY.as_slice(),
        reference.as_slice(),
        "fixture body must equal the generated TL serialization"
    );
    let body = RAW_BODY.to_vec();
    let body_base64 = v6_executor::encode_base64(&body);
    let marker = "marker.txt";
    let script = r#"#!/usr/bin/env python3
import sys, json, base64, os

def write_marker(content):
    # Atomic visibility: the marker path only appears with full content, so a
    # poller can never observe a partially-buffered write.
    tmp = "__MARKER__.tmp"
    with open(tmp, "w") as f:
        f.write(content)
    os.replace(tmp, "__MARKER__")

def req_id(line):
    return json.loads(line)["request_id"]

try:
    # 1. initialize
    line = sys.stdin.readline()
    if not line:
        sys.exit(60)
    frame = json.loads(line)
    assert frame["type"] == "initialize", frame
    rid = req_id(line)
    print(json.dumps({"protocol_version": 6, "type": "initialized", "request_id": rid, "module_id": "__MODULE_ID__"}), flush=True)

    # 2. execute
    line = sys.stdin.readline()
    if not line:
        sys.exit(61)
    frame = json.loads(line)
    assert frame["type"] == "execute", frame
    assert frame["command"] == "go", frame.get("command")
    assert frame["arguments"] == "packaged", frame.get("arguments")
    rid = req_id(line)
    print(json.dumps({"protocol_version": 6, "type": "result", "request_id": rid, "text": "packaged ok"}), flush=True)

    # 3. event dispatch
    line = sys.stdin.readline()
    if not line:
        sys.exit(63)
    frame = json.loads(line)
    assert frame["type"] == "event", frame
    assert frame["event"] == "message.created", frame.get("event")
    assert frame["payload"]["text"] == "triggered", frame.get("payload")
    rid = req_id(line)
    print(json.dumps({"protocol_version": 6, "type": "event_result", "request_id": rid, "actions": []}), flush=True)

    # 4. curated Telegram RPC
    print('{"protocol_version":6,"type":"telegram.invoke","call_id":"cur-1","method":"messages.getDialogs","params":{"limit":10,"hash":"0"}}', flush=True)
    line = sys.stdin.readline()
    if not line:
        sys.exit(64)
    frame = json.loads(line)
    assert frame["type"] == "telegram.result", frame
    assert frame["call_id"] == "cur-1", frame.get("call_id")
    result = frame.get("result")
    assert result is not None, frame
    assert result["kind"] == "dialogs_summary", result.get("kind")
    assert result["dialogs_count"] == 2, result.get("dialogs_count")
    assert result["truncated"] is False, result.get("truncated")

    # 5. raw.invoke with a fully serialized valid TL function
    print('{"protocol_version":6,"type":"telegram.invoke","call_id":"raw-1","method":"raw.invoke","params":{"dc_id":2,"body_base64_chunks":["__BODY_BASE64__"]}}', flush=True)
    line = sys.stdin.readline()
    if not line:
        sys.exit(65)
    frame = json.loads(line)
    assert frame["type"] == "telegram.result", frame
    assert frame["call_id"] == "raw-1", frame.get("call_id")
    result = frame.get("result")
    assert result is not None, frame
    assert result["kind"] == "raw_tl", result.get("kind")
    assert result["dc_id"] == 2, result.get("dc_id")
    decoded = base64.b64decode("".join(result["body_base64_chunks"]))
    assert decoded == b"opaque-response-ok", decoded
    write_marker("all-stages-ok")

    # 6. health
    line = sys.stdin.readline()
    if not line:
        sys.exit(66)
    frame = json.loads(line)
    assert frame["type"] == "health", frame
    rid = req_id(line)
    print(json.dumps({"protocol_version": 6, "type": "health", "request_id": rid}), flush=True)

    # 7. graceful shutdown
    line = sys.stdin.readline()
    if not line:
        sys.exit(67)
    frame = json.loads(line)
    assert frame["type"] == "shutdown", frame
except Exception as error:
    write_marker("FAILED: " + repr(error))
    sys.exit(62)
sys.exit(0)
"#
        .replace("__MODULE_ID__", &module_id)
        .replace("__BODY_BASE64__", &body_base64)
        .replace("__MARKER__", marker);

    // The packaged manifest grants one curated helper (which must not
    // require `telegram.raw`) and `raw.invoke` (which must).
    let manifest_json = format!(
        r#"{{"schema_version":6,"id":"{module_id}","name":"Packaged","version":"1","author":"A","entrypoint":"run","capabilities":["telegram.raw"],"telegram_methods":["messages.getDialogs","raw.invoke"],"commands":[{{"name":"go","summary_ru":"x","description_ru":"x","usage":"<value>"}}]}}"#
    );
    let archive = zip_entries(&[
        ArchiveEntry {
            name: "module.json".to_owned(),
            data: manifest_json.into_bytes(),
            mode: 0o100644,
        },
        ArchiveEntry {
            name: "run".to_owned(),
            data: fixture_script(&script),
            mode: 0o100755,
        },
    ]);

    // Production inspection pipeline: staging, zip inspection, manifest
    // validation, then the validated descriptor.
    let config = InspectionConfig {
        staging_root: root.clone(),
        limits: InspectionLimits::default(),
    };
    let mut inspector = ModuleInspector::new(&config, OsRandom);
    let pending = inspector
        .inspect(
            AcquiredLmod::archive(archive),
            std::time::SystemTime::now(),
            std::time::SystemTime::now(),
        )
        .expect("packaged module must pass inspection");
    let payload = pending.stage.take_wrapper().unwrap().join("payload");
    let descriptor = manifest::validate_manifest_at(&payload.join("module.json"), Some(&module_id))
        .expect("production manifest validation");
    assert_eq!(descriptor.protocol_version, 6);
    assert_eq!(
        descriptor.telegram_methods,
        vec![
            v6_registry::V6Method::MessagesGetDialogs,
            v6_registry::V6Method::RawInvoke
        ]
    );

    ensure_test_state_base();
    let transport = FakeRawTlTransport {
        home_dc: Some(2),
        response: b"opaque-response-ok".to_vec(),
        received: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let received = transport.received.clone();
    let executor = Arc::new(PackagedExecutor {
        transport: Arc::new(transport),
        curated: Default::default(),
    });
    let observed_curated = executor.curated.clone();
    let v6proc = V6Process::start(descriptor, executor, 1).await.unwrap();

    // Stage 1: initialize
    v6proc
        .initialize("1".to_owned(), module_id.clone())
        .await
        .unwrap();

    // Stage 2: execute
    let reply = v6proc
        .execute(
            "2".to_owned(),
            "go".to_owned(),
            "packaged".to_owned(),
            vec![],
        )
        .await
        .unwrap();
    assert!(matches!(reply, V6InboundFrame::Result { ref text, .. } if text == "packaged ok"));

    // Stage 3: event dispatch
    let event_reply = v6proc
        .event(
            "3".to_owned(),
            MessageEventKind::Created,
            MessageEvent {
                event_id: "e1".to_owned(),
                message_ref: "r1".to_owned(),
                message_key: "k1".to_owned(),
                peer_id: None,
                text: "triggered".to_owned(),
                outgoing: false,
                entities: vec![],
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(event_reply, V6InboundFrame::EventResult { ref actions, .. } if actions.is_empty())
    );

    // Stages 4-5 run inside the module: it issues the curated RPC, then
    // raw.invoke, verifies both results against expectations, and writes
    // its marker only after both round-trips succeeded.
    let marker_path = payload.join(marker);
    let mut verified = false;
    for _ in 0..300 {
        if marker_path.exists() {
            verified = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        verified,
        "module must complete the curated and raw RPC stages"
    );
    let marker_text = fs::read_to_string(&marker_path).unwrap();
    assert!(
        !marker_text.starts_with("FAILED"),
        "module rejected a stage: {marker_text}"
    );
    assert_eq!(marker_text, "all-stages-ok");

    // The curated call passed through the normal executor contract with
    // the exact granted schema.
    {
        let curated = observed_curated.lock().unwrap();
        assert_eq!(curated.len(), 1);
        assert_eq!(curated[0].0, v6_registry::V6Method::MessagesGetDialogs);
        assert_eq!(curated[0].1, serde_json::json!({"limit": 10, "hash": "0"}));
    }

    // Raw bytes: the exact module-produced serialized function reached the
    // transport unchanged, and the exact fake response reached the module
    // unchanged (asserted by the module before its marker write).
    {
        let received = received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].0, 2);
        assert_eq!(received[0].1, RAW_BODY);
    }

    // Stage 6: health
    v6proc.health("6".to_owned()).await.unwrap();

    // Stage 7: graceful shutdown
    assert!(v6proc.graceful_shutdown().await.is_ok());
    assert_eq!(v6proc.status(), process::ProcessStatus::Terminated);
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn nonzero_shutdown_is_crash_and_retains_exit_and_stderr() {
    use std::fs;
    let root = test_root("v6-bad-shutdown");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let entrypoint = root.join("run");
    ensure_test_state_base();
    write_v6_fixture(
        &entrypoint,
        "#!/usr/bin/env python3\nimport sys, os\nsys.stdin.readline()\nprint('shutdown-broke', file=sys.stderr)\nsys.stderr.flush()\nsys.exit(7)\n",
    );

    let mut module = descriptor();
    module.id = format!("v6badshutdown{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = entrypoint;
    let v6proc = V6Process::start(module, Arc::new(RecordingExecutor::default()), 1)
        .await
        .unwrap();

    assert!(matches!(
        v6proc.graceful_shutdown().await,
        Err(ExternalError::Unavailable)
    ));
    for _ in 0..200 {
        if v6proc.status() == process::ProcessStatus::Crashed && v6proc.diagnostic().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(v6proc.status(), process::ProcessStatus::Crashed);
    let diagnostic = v6proc.diagnostic().expect("crash diagnostic");
    assert_eq!(diagnostic.lifecycle_stage, "shutdown");
    assert_eq!(diagnostic.exit_code, Some(7));
    assert_eq!(diagnostic.error_category, "unavailable");
    assert!(diagnostic.stderr.contains("shutdown-broke"));
    assert!(diagnostic.timestamp_unix_ms > 0);
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn force_terminate_kills_process_and_reports_terminated() {
    use std::fs;
    let root = test_root("v6-force");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let entrypoint = root.join("run");
    ensure_test_state_base();
    write_v6_fixture(
        &entrypoint,
        "#!/usr/bin/env python3\nimport os, time\nwith open('leader.pid', 'w') as f:\n    f.write(str(os.getpid()))\ntime.sleep(30)\n",
    );

    let mut module = descriptor();
    module.id = format!("v6force{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = entrypoint;
    let v6proc = V6Process::start(module, Arc::new(RecordingExecutor::default()), 1)
        .await
        .unwrap();
    let pid_path = root.join("leader.pid");
    for _ in 0..100 {
        if pid_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let leader: i32 = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    v6proc.terminate().await;
    assert_eq!(v6proc.status(), process::ProcessStatus::Terminated);
    assert!(v6proc.diagnostic().is_none());
    let proc_stat = fs::read_to_string(format!("/proc/{leader}/stat"));
    let live = match proc_stat {
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_none_or(|(_, tail)| !tail.starts_with('Z')),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    };
    let _ = fs::remove_dir_all(&root);
    assert!(!live, "force-terminated module leader remained alive");
}

#[cfg(unix)]
#[tokio::test]
async fn invalid_event_result_is_fatal_at_the_v6_boundary() {
    use std::fs;
    let root = test_root("v6-bad-event");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let entrypoint = root.join("run");
    ensure_test_state_base();
    write_v6_fixture(
        &entrypoint,
        "#!/usr/bin/env python3\nimport sys, time, json\nline = sys.stdin.readline()\nrid = json.loads(line)[\"request_id\"]\nprint(json.dumps({\"protocol_version\": 6, \"type\": \"initialized\", \"request_id\": rid, \"module_id\": \"__MODULE_ID__\"}), flush=True)\nline = sys.stdin.readline()\nrid = json.loads(line)[\"request_id\"]\n# Malformed action object: the schema must be rejected at the v6 inbound\n# boundary, before any lifecycle request can be reported as completed.\nprint(json.dumps({\"protocol_version\": 6, \"type\": \"event_result\", \"request_id\": rid, \"actions\": [{\"type\": \"text.send\", \"message_ref\": \"r1\"}]}), flush=True)\ntime.sleep(30)\n",
    );

    let mut module = descriptor();
    module.id = format!("v6badevent{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = entrypoint;
    let script = "#!/usr/bin/env python3\nimport sys, time, json\nline = sys.stdin.readline()\nrid = json.loads(line)[\"request_id\"]\nprint(json.dumps({\"protocol_version\": 6, \"type\": \"initialized\", \"request_id\": rid, \"module_id\": \"__MODULE_ID__\"}), flush=True)\nline = sys.stdin.readline()\nrid = json.loads(line)[\"request_id\"]\n# Malformed action object: the schema must be rejected at the v6 inbound\n# boundary, before any lifecycle request can be reported as completed.\nprint(json.dumps({\"protocol_version\": 6, \"type\": \"event_result\", \"request_id\": rid, \"actions\": [{\"type\": \"text.send\", \"message_ref\": \"r1\"}]}), flush=True)\ntime.sleep(30)\n"
            .replace("__MODULE_ID__", &module.id);
    write_v6_fixture(&module.entrypoint, &script);
    let v6proc = V6Process::start(module.clone(), Arc::new(RecordingExecutor::default()), 1)
        .await
        .unwrap();
    v6proc
        .initialize("1".to_owned(), module.id.clone())
        .await
        .unwrap();
    assert!(matches!(
        v6proc
            .event(
                "2".to_owned(),
                MessageEventKind::Created,
                MessageEvent {
                    event_id: "e".to_owned(),
                    message_ref: "r".to_owned(),
                    message_key: "k".to_owned(),
                    peer_id: None,
                    text: "hello".to_owned(),
                    outgoing: false,
                    entities: vec![],
                },
            )
            .await,
        Err(ExternalError::ProtocolDecode)
    ));
    for _ in 0..200 {
        if v6proc.diagnostic().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let diagnostic = v6proc.diagnostic().expect("protocol diagnostic");
    assert_eq!(diagnostic.lifecycle_stage, "event");
    assert_eq!(diagnostic.error_category, "protocol_decode");
    assert_eq!(v6proc.status(), process::ProcessStatus::Crashed);
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn start_failure_carries_spawn_diagnostic() {
    let root = test_root("v6-start-failure");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    ensure_test_state_base();
    let mut module = descriptor();
    module.id = format!("v6startfailure{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = root.join("missing-entrypoint");
    let failure = match V6Process::start(module, Arc::new(RecordingExecutor::default()), 7).await {
        Ok(_) => panic!("missing entrypoint must fail before publishing a process"),
        Err(failure) => failure,
    };
    assert_eq!(failure.diagnostics.lifecycle_stage, "spawn");
    assert_eq!(failure.diagnostics.error_category, "unavailable");
    assert_eq!(failure.diagnostics.restart_generation, 7);
    assert_eq!(failure.diagnostics.exit_code, None);
    assert_eq!(failure.diagnostics.signal, None);
    assert!(failure.diagnostics.stderr.is_empty());
    assert!(failure.diagnostics.timestamp_unix_ms > 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_requests_are_serialized_but_parentless_rpc_is_not_blocked() {
    use std::fs;
    let root = test_root("v6-sequential");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    ensure_test_state_base();
    let marker = root.join("marker");
    let mut module = descriptor();
    module.id = format!("v6sequential{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = root.join("run");
    module.telegram_methods = vec![v6_registry::V6Method::ContactsGetContacts];
    let script = r#"#!/usr/bin/env python3
import json, os, select, sys, time

line = sys.stdin.readline()
rid = json.loads(line)["request_id"]
with open("marker", "w") as f:
    f.write("first-received")
time.sleep(0.25)
ready, _, _ = select.select([sys.stdin], [], [], 0.05)
with open("marker", "a") as f:
    f.write("|" + ("second-too-early" if ready else "no-second-before-response"))
print(json.dumps({"protocol_version":6,"type":"initialized","request_id":rid,"module_id":"__MODULE_ID__"}), flush=True)

line = sys.stdin.readline()
frame = json.loads(line)
assert frame["type"] == "execute"
rid = frame["request_id"]
print(json.dumps({"protocol_version":6,"type":"telegram.invoke","call_id":"during-execute","method":"contacts.getContacts","params":{"hash":"0"}}), flush=True)
result = json.loads(sys.stdin.readline())
assert result["type"] == "telegram.result"
assert result["call_id"] == "during-execute"
print(json.dumps({"protocol_version":6,"type":"result","request_id":rid,"text":"ok"}), flush=True)
sys.stdin.readline()
sys.exit(0)
"#
            .replace("__MODULE_ID__", &module.id);
    write_v6_fixture(&module.entrypoint, &script);
    let executor = Arc::new(RecordingExecutor::default());
    let observed = executor.clone();
    let process = V6Process::start(module.clone(), executor, 1).await.unwrap();
    let init_process = process.clone();
    let init =
        tokio::spawn(async move { init_process.initialize("1".to_owned(), module.id).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let execute_process = process.clone();
    let execute = tokio::spawn(async move {
        execute_process
            .execute("2".to_owned(), "go".to_owned(), String::new(), vec![])
            .await
    });
    assert!(matches!(
        init.await.unwrap(),
        Ok(V6InboundFrame::Initialized { .. })
    ));
    assert!(
        matches!(execute.await.unwrap(), Ok(V6InboundFrame::Result { ref text, .. }) if text == "ok")
    );
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        "first-received|no-second-before-response"
    );
    assert_eq!(
        observed.methods.lock().unwrap().as_slice(),
        &[v6_registry::V6Method::ContactsGetContacts]
    );
    process.graceful_shutdown().await.unwrap();
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_waiting_queue_returns_backpressure_when_full() {
    use std::fs;
    let root = test_root("v6-queue-full");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    ensure_test_state_base();
    let mut module = descriptor();
    module.id = format!("v6queuefull{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = root.join("run");
    let script = r#"#!/usr/bin/env python3
import os, sys, time
sys.stdin.readline()
open("received", "w").close()
time.sleep(30)
"#;
    write_v6_fixture(&module.entrypoint, script);
    let process = V6Process::start(module.clone(), Arc::new(RecordingExecutor::default()), 1)
        .await
        .unwrap();
    let initialize = {
        let process = process.clone();
        let id = module.id.clone();
        tokio::spawn(async move { process.initialize("1".to_owned(), id).await })
    };
    for _ in 0..100 {
        if root.join("received").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(root.join("received").exists());
    let mut queued = Vec::new();
    for number in 0..V6_MAX_PENDING {
        let process = process.clone();
        queued.push(tokio::spawn(async move {
            process
                .execute(
                    (number + 2).to_string(),
                    "go".to_owned(),
                    String::new(),
                    vec![],
                )
                .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(matches!(
        process
            .execute("99".to_owned(), "go".to_owned(), String::new(), vec![],)
            .await,
        Err(ExternalError::Backpressure)
    ));
    process.terminate().await;
    assert!(matches!(
        initialize.await.unwrap(),
        Err(ExternalError::Unavailable)
    ));
    for queued in queued {
        assert!(matches!(
            queued.await.unwrap(),
            Err(ExternalError::Unavailable)
        ));
    }
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn force_terminate_bypasses_lifecycle_queue() {
    use std::fs;
    let root = test_root("v6-force-queue");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    ensure_test_state_base();
    let mut module = descriptor();
    module.id = format!("v6forcequeue{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = root.join("run");
    write_v6_fixture(
        &module.entrypoint,
        "#!/usr/bin/env python3\nimport sys, time\nsys.stdin.readline()\nopen('received', 'w').close()\ntime.sleep(30)\n",
    );
    let process = V6Process::start(module.clone(), Arc::new(RecordingExecutor::default()), 1)
        .await
        .unwrap();
    let initialize = {
        let process = process.clone();
        let id = module.id.clone();
        tokio::spawn(async move { process.initialize("1".to_owned(), id).await })
    };
    for _ in 0..100 {
        if root.join("received").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let queued = {
        let process = process.clone();
        tokio::spawn(async move {
            process
                .execute("2".to_owned(), "go".to_owned(), String::new(), vec![])
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    tokio::time::timeout(Duration::from_secs(1), process.terminate())
        .await
        .expect("force termination must not wait behind lifecycle work");
    assert_eq!(process.status(), process::ProcessStatus::Terminated);
    assert!(matches!(
        initialize.await.unwrap(),
        Err(ExternalError::Unavailable)
    ));
    assert!(matches!(
        queued.await.unwrap(),
        Err(ExternalError::Unavailable)
    ));
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_drains_queued_lifecycle_replies_without_writing_them() {
    use std::fs;
    let root = test_root("v6-shutdown-queue");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    ensure_test_state_base();
    let mut module = descriptor();
    module.id = format!("v6shutdownqueue{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = root.join("run");
    write_v6_fixture(
        &module.entrypoint,
        "#!/usr/bin/env python3\nimport json, sys\nsys.stdin.readline()\nopen('received', 'w').close()\nframe = json.loads(sys.stdin.readline())\nassert frame['type'] == 'shutdown', frame\nopen('shutdown', 'w').close()\nsys.exit(0)\n",
    );
    let process = V6Process::start(module.clone(), Arc::new(RecordingExecutor::default()), 1)
        .await
        .unwrap();
    let initialize = {
        let process = process.clone();
        let id = module.id.clone();
        tokio::spawn(async move { process.initialize("1".to_owned(), id).await })
    };
    for _ in 0..100 {
        if root.join("received").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let queued = {
        let process = process.clone();
        tokio::spawn(async move {
            process
                .execute("2".to_owned(), "go".to_owned(), String::new(), vec![])
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    process.graceful_shutdown().await.unwrap();
    assert!(root.join("shutdown").exists());
    assert_eq!(process.status(), process::ProcessStatus::Terminated);
    assert!(matches!(
        initialize.await.unwrap(),
        Err(ExternalError::Unavailable)
    ));
    assert!(matches!(
        queued.await.unwrap(),
        Err(ExternalError::Unavailable)
    ));
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn malformed_child_frame_crashes_with_retained_protocol_diagnostic() {
    use std::fs;
    let root = test_root("v6-malformed");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let entrypoint = root.join("run");
    ensure_test_state_base();
    write_v6_fixture(
        &entrypoint,
        "#!/usr/bin/env python3\nimport sys, time\nsys.stdin.readline()\nprint('malformed-frame', file=sys.stderr)\nsys.stderr.flush()\nprint('{broken-json')\nsys.stdout.flush()\ntime.sleep(30)\n",
    );

    let mut module = descriptor();
    module.id = format!("v6malformed{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = entrypoint;
    let v6proc = V6Process::start(module.clone(), Arc::new(RecordingExecutor::default()), 1)
        .await
        .unwrap();
    assert!(matches!(
        v6proc.initialize("1".to_owned(), module.id).await,
        Err(ExternalError::ProtocolDecode)
    ));
    for _ in 0..200 {
        if v6proc.diagnostic().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let diagnostic = v6proc.diagnostic().expect("protocol diagnostic");
    assert_eq!(diagnostic.lifecycle_stage, "initialize");
    assert_eq!(diagnostic.error_category, "protocol_decode");
    assert!(diagnostic.stderr.contains("malformed-frame"));
    assert_eq!(v6proc.status(), process::ProcessStatus::Crashed);
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_timeout_is_fatal_and_retained() {
    use std::fs;
    let root = test_root("v6-timeout");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let entrypoint = root.join("run");
    ensure_test_state_base();
    write_v6_fixture(
        &entrypoint,
        "#!/usr/bin/env python3\nimport sys, time\nsys.stdin.readline()\ntime.sleep(30)\n",
    );

    let mut module = descriptor();
    module.id = format!("v6timeout{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = entrypoint;
    let v6proc = V6Process::start(module.clone(), Arc::new(RecordingExecutor::default()), 1)
        .await
        .unwrap();
    assert!(matches!(
        v6proc.initialize("1".to_owned(), module.id).await,
        Err(ExternalError::ExecutionTimeout)
    ));
    for _ in 0..200 {
        if v6proc.diagnostic().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let diagnostic = v6proc.diagnostic().expect("timeout diagnostic");
    assert_eq!(diagnostic.lifecycle_stage, "initialize");
    assert_eq!(diagnostic.error_category, "execution_timeout");
    assert_eq!(v6proc.status(), process::ProcessStatus::Crashed);
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn real_child_conformance_covers_lifecycle_curated_and_raw_rpc() {
    use std::fs;
    let root = test_root("v6-conformance");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let entrypoint = root.join("run");

    let mut module = descriptor();
    module.id = format!("v6conformance{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = entrypoint.clone();
    module.telegram_methods = vec![
        v6_registry::V6Method::ContactsGetContacts,
        v6_registry::V6Method::RawInvoke,
    ];
    module
        .capabilities
        .push(manifest::ExternalCapability::TelegramRaw);

    ensure_test_state_base();
    let script = r#"#!/usr/bin/env python3
import sys, json

def req_id(line):
    return json.loads(line)["request_id"]

line = sys.stdin.readline()
if not line:
    sys.exit(80)
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "initialized", "request_id": rid, "module_id": "__MODULE_ID__"}), flush=True)
line = sys.stdin.readline()
if not line:
    sys.exit(81)
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "result", "request_id": rid, "text": "ok"}), flush=True)
line = sys.stdin.readline()
if not line:
    sys.exit(82)
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "event_result", "request_id": rid, "actions": []}), flush=True)
print('{"protocol_version":6,"type":"telegram.invoke","call_id":"curated-1","method":"contacts.getContacts","params":{"hash":"0"}}', flush=True)
line = sys.stdin.readline()
if not line:
    sys.exit(83)
print('{"protocol_version":6,"type":"telegram.invoke","call_id":"raw-1","method":"raw.invoke","params":{"body_base64_chunks":["eFY0EgEAAAA="]}}', flush=True)
line = sys.stdin.readline()
if not line:
    sys.exit(84)
line = sys.stdin.readline()
if not line:
    sys.exit(85)
rid = req_id(line)
print(json.dumps({"protocol_version": 6, "type": "health", "request_id": rid}), flush=True)
line = sys.stdin.readline()
if not line:
    sys.exit(86)
sys.exit(0)
"#
        .replace("__MODULE_ID__", &module.id);
    write_v6_fixture(&entrypoint, &script);

    let executor = Arc::new(RecordingExecutor::default());
    let v6proc = V6Process::start(module.clone(), executor.clone(), 1)
        .await
        .unwrap();
    assert!(matches!(
        v6proc
            .initialize("1".to_owned(), module.id.clone())
            .await
            .unwrap(),
        V6InboundFrame::Initialized { .. }
    ));
    assert!(matches!(
        v6proc.execute("2".to_owned(), "run".to_owned(), String::new(), vec![]).await.unwrap(),
        V6InboundFrame::Result { text, .. } if text == "ok"
    ));
    assert!(matches!(
        v6proc
            .event(
                "3".to_owned(),
                MessageEventKind::Created,
                MessageEvent {
                    event_id: "e".to_owned(),
                    message_ref: "r".to_owned(),
                    message_key: "k".to_owned(),
                    peer_id: None,
                    text: "hello".to_owned(),
                    outgoing: false,
                    entities: vec![],
                },
            )
            .await
            .unwrap(),
        V6InboundFrame::EventResult { .. }
    ));
    for _ in 0..200 {
        if executor.methods.lock().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        *executor.methods.lock().unwrap(),
        vec![
            v6_registry::V6Method::ContactsGetContacts,
            v6_registry::V6Method::RawInvoke
        ]
    );
    assert!(matches!(
        v6proc.health("4".to_owned()).await.unwrap(),
        V6InboundFrame::Health { .. }
    ));
    v6proc.graceful_shutdown().await.unwrap();
    for _ in 0..100 {
        if v6proc.status() == process::ProcessStatus::Terminated {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(v6proc.status(), process::ProcessStatus::Terminated);
    assert!(v6proc.diagnostic().is_none());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ungranted_method_is_rejected_before_executor() {
    let descriptor = descriptor();
    assert!(
        matches!(validate_invoke(&descriptor, "account.updateStatus", true), Err(V6CallError { kind, .. }) if kind == "capability")
    );
}

#[test]
fn registry_capability_policy_gates_runtime_invoke() {
    // The runtime gate reads the same generated-registry policy the
    // installer uses: granted-but-missing-capability is rejected, and the
    // presence of the required capability is accepted.
    let mut descriptor = descriptor();
    descriptor.telegram_methods = vec![v6_registry::V6Method::AccountUpdateStatus];
    assert!(
        matches!(validate_invoke(&descriptor, "account.updateStatus", true), Err(V6CallError { kind, .. }) if kind == "capability")
    );
    descriptor
        .capabilities
        .push(manifest::ExternalCapability::TelegramAccountStatus);
    assert_eq!(
        validate_invoke(&descriptor, "account.updateStatus", true).unwrap(),
        v6_registry::V6Method::AccountUpdateStatus
    );
}

#[test]
fn curated_methods_do_not_require_raw_but_raw_does() {
    let mut descriptor = descriptor();
    descriptor.telegram_methods = vec![v6_registry::V6Method::ContactsGetContacts];
    assert_eq!(
        validate_invoke(&descriptor, "contacts.getContacts", true).unwrap(),
        v6_registry::V6Method::ContactsGetContacts
    );

    descriptor.telegram_methods = vec![v6_registry::V6Method::RawInvoke];
    assert!(matches!(
        validate_invoke(&descriptor, "raw.invoke", true),
        Err(V6CallError { kind, .. }) if kind == "capability"
    ));
    descriptor
        .capabilities
        .push(manifest::ExternalCapability::TelegramRaw);
    assert_eq!(
        validate_invoke(&descriptor, "raw.invoke", true).unwrap(),
        v6_registry::V6Method::RawInvoke
    );
}

#[test]
fn terminal_frames_preserve_their_request_id() {
    let frame = V6InboundFrame::Health {
        request_id: "12".to_owned(),
    };
    assert_eq!(terminal_request_id(&frame), Some("12"));
    assert!(expected_matches(Expected::Health, &frame));
}

#[test]
fn initialized_module_id_must_match_descriptor() {
    let frame = V6InboundFrame::Initialized {
        request_id: "12".to_owned(),
        module_id: "other".to_owned(),
    };
    assert!(!initialized_module_matches(&descriptor(), &frame));
}

#[test]
fn executor_errors_are_sanitized() {
    assert_eq!(
        v6_executor_error(V6ExecutorError::InvalidParams("secret")),
        V6CallError {
            kind: "validation".to_owned(),
            message: "invalid parameters".to_owned(),
            code: None,
            name: None,
            retry_after_seconds: None,
        }
    );
    assert_eq!(v6_executor_error(V6ExecutorError::Timeout).kind, "timeout");
    assert_eq!(
        v6_executor_error(V6ExecutorError::Rpc {
            code: 420,
            name: "FLOOD_WAIT".to_owned(),
            retry_after_seconds: Some(7)
        })
        .kind,
        "rpc"
    );
}

#[test]
fn shutdown_requires_a_flushed_shutdown_frame_before_child_exit() {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let exited = Ok(std::process::ExitStatus::from_raw(0));
        assert!(matches!(
            shutdown_child_exit_result(false, exited),
            Err(ExternalError::Unavailable)
        ));
        assert!(
            shutdown_child_exit_result(true, Ok(std::process::ExitStatus::from_raw(0))).is_ok()
        );
        assert!(matches!(
            shutdown_child_exit_result(true, Ok(std::process::ExitStatus::from_raw(1 << 8))),
            Err(ExternalError::Unavailable)
        ));
    }
    assert!(!reader_eof_is_fatal(true));
    assert!(reader_eof_is_fatal(false));
}

#[test]
fn call_ids_remain_unique_while_closing() {
    let mut active = HashSet::new();
    assert!(reserve_call_id(&mut active, "call"));
    assert!(!reserve_call_id(&mut active, "call"));
}

#[test]
fn lifecycle_queue_failure_drains_in_flight_and_waiting_requests() {
    let (reply, mut receiver) = oneshot::channel();
    let mut in_flight = Some(Pending {
        request_id: "1".to_owned(),
        expected: Expected::Health,
        deadline: Some(Instant::now() - Duration::from_secs(1)),
        reply,
    });
    let (queued_reply, mut queued_receiver) = oneshot::channel();
    let mut waiting = VecDeque::from([QueuedRequest {
        frame: V6OutboundCoreFrame::Health {
            request_id: "2".to_owned(),
        },
        expected: Expected::Health,
        reply: queued_reply,
    }]);
    fail_lifecycle_with(&mut in_flight, &mut waiting, FatalReason::ExecutionTimeout);
    assert!(in_flight.is_none());
    assert!(waiting.is_empty());
    assert!(matches!(
        receiver.try_recv(),
        Ok(Err(ExternalError::ExecutionTimeout))
    ));
    assert!(matches!(
        queued_receiver.try_recv(),
        Ok(Err(ExternalError::ExecutionTimeout))
    ));
}

#[tokio::test]
async fn queued_request_has_no_deadline_until_dispatch() {
    // The sequential gate: a queued (not yet sent) lifecycle request must
    // not carry a deadline. Its timeout begins only after the writer
    // acknowledges the completed transport flush.
    let (writer, mut writer_rx) = mpsc::channel(8);
    let (reply, receiver) = oneshot::channel();
    let mut in_flight = None;
    let enqueued_at = Instant::now();
    let mut waiting = VecDeque::from([QueuedRequest {
        frame: V6OutboundCoreFrame::Health {
            request_id: "7".to_owned(),
        },
        expected: Expected::Health,
        reply,
    }]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let queued = waiting.pop_front().expect("queued request");
    let mut awaiting_lifecycle_flush = HashSet::new();
    dispatch_lifecycle(
        &mut in_flight,
        &mut awaiting_lifecycle_flush,
        &writer,
        queued.frame,
        queued.expected,
        queued.reply,
    )
    .expect("dispatch must submit the frame");
    let dispatched = in_flight.expect("in flight after dispatch");
    assert_eq!(dispatched.request_id, "7");
    assert!(dispatched.deadline.is_none());
    let mut in_flight = Some(dispatched);
    assert!(awaiting_lifecycle_flush.contains("7"));
    let acknowledgement_at = Instant::now();
    handle_lifecycle_flushed(&mut in_flight, &mut awaiting_lifecycle_flush, "7")
        .expect("matching flush acknowledgement");
    assert!(!awaiting_lifecycle_flush.contains("7"));
    let deadline = in_flight
        .as_ref()
        .and_then(|request| request.deadline)
        .expect("flush acknowledgement starts deadline");
    // The deadline is anchored at the flush acknowledgement, not enqueue.
    let expected = acknowledgement_at + lifecycle_timeout();
    let window = Duration::from_millis(50);
    assert!(
        deadline >= expected - window && deadline <= expected + window,
        "deadline must start at flush acknowledgement"
    );
    assert!(
        deadline > enqueued_at + lifecycle_timeout() + window,
        "queued time must not consume the lifecycle deadline"
    );
    // The frame reached the writer queue exactly once.
    assert!(matches!(
        writer_rx.try_recv(),
        Ok(WriterCommand::Frame(
            V6OutboundCoreFrame::Health { .. },
            Flush::Lifecycle(_),
        ))
    ));
    assert!(writer_rx.try_recv().is_err());
    drop(receiver);
}

#[test]
fn lifecycle_flush_ack_requires_matching_non_duplicate_request_id() {
    let (reply, _receiver) = oneshot::channel();
    let mut in_flight = Some(Pending {
        request_id: "7".to_owned(),
        expected: Expected::Health,
        deadline: None,
        reply,
    });
    let mut awaiting_lifecycle_flush = HashSet::from(["7".to_owned()]);
    assert!(matches!(
        handle_lifecycle_flushed(&mut in_flight, &mut awaiting_lifecycle_flush, "8"),
        Err(FatalReason::WrongRequestId)
    ));
    handle_lifecycle_flushed(&mut in_flight, &mut awaiting_lifecycle_flush, "7")
        .expect("matching acknowledgement");
    assert!(matches!(
        handle_lifecycle_flushed(&mut in_flight, &mut awaiting_lifecycle_flush, "7"),
        Err(FatalReason::WrongRequestId)
    ));
}

#[test]
fn response_before_flush_ack_consumes_the_late_ack() {
    let mut in_flight = None;
    let mut awaiting = HashSet::from(["7".to_owned()]);
    handle_lifecycle_flushed(&mut in_flight, &mut awaiting, "7")
        .expect("valid response-before-ack ordering");
    assert!(awaiting.is_empty());
}

#[test]
fn shutdown_before_flush_ack_consumes_the_valid_ack_without_crashing() {
    let mut in_flight = None;
    let mut awaiting = HashSet::from(["7".to_owned()]);
    // This models fail_lifecycle() having cancelled the caller before the
    // writer acknowledgement event was selected by the supervisor.
    handle_lifecycle_flushed(&mut in_flight, &mut awaiting, "7")
        .expect("shutdown cancellation must not reject a valid ack");
    assert!(awaiting.is_empty());
    assert!(matches!(
        handle_lifecycle_flushed(&mut in_flight, &mut awaiting, "7"),
        Err(FatalReason::WrongRequestId)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_cancellation_consumes_delayed_lifecycle_ack_before_clean_exit() {
    use std::os::unix::process::ExitStatusExt;

    let (lifecycle_reply, mut lifecycle_response) = oneshot::channel();
    let mut in_flight = Some(Pending {
        request_id: "7".to_owned(),
        expected: Expected::Health,
        deadline: None,
        reply: lifecycle_reply,
    });
    let mut waiting = VecDeque::new();
    let mut awaiting = HashSet::from(["7".to_owned()]);
    let mut workers = JoinSet::new();
    let (writer, mut writer_rx) = mpsc::channel(1);
    let (shutdown_reply, _shutdown_response) = oneshot::channel();
    let mut saved_shutdown_reply = None;
    let mut cancelled_lifecycle_request = None;
    let mut closing = false;

    // Exact controlled ordering: lifecycle write has flushed, shutdown
    // cancels its caller, then the delayed internal ack is consumed.
    start_shutdown(
        &mut closing,
        &mut in_flight,
        &mut waiting,
        &mut workers,
        &writer,
        "8".to_owned(),
        Some(shutdown_reply),
        &mut saved_shutdown_reply,
        &mut cancelled_lifecycle_request,
    )
    .expect("shutdown submission");
    assert!(closing);
    assert!(in_flight.is_none());
    assert_eq!(cancelled_lifecycle_request.as_deref(), Some("7"));
    assert!(matches!(
        lifecycle_response.try_recv(),
        Ok(Err(ExternalError::Unavailable))
    ));
    assert!(matches!(
        writer_rx.try_recv(),
        Ok(WriterCommand::Frame(
            V6OutboundCoreFrame::Shutdown { .. },
            Flush::Shutdown
        ))
    ));
    handle_lifecycle_flushed(&mut in_flight, &mut awaiting, "7")
        .expect("delayed valid ack is consumed after shutdown cancellation");
    assert!(awaiting.is_empty());
    assert!(shutdown_child_exit_result(true, Ok(std::process::ExitStatus::from_raw(0))).is_ok());
    assert!(saved_shutdown_reply.is_some());
}

#[test]
fn request_id_reuse_is_blocked_until_flush_ack_consumption() {
    let in_flight = None;
    let waiting = VecDeque::new();
    let awaiting = HashSet::from(["7".to_owned()]);
    assert!(lifecycle_request_id_present(
        &in_flight, &waiting, &awaiting, "7"
    ));
    assert!(!lifecycle_request_id_present(
        &in_flight, &waiting, &awaiting, "8"
    ));
}

#[test]
fn writer_failure_before_lifecycle_flush_ack_replies_and_fails() {
    let (writer, _reader) = mpsc::channel(1);
    writer
        .try_send(WriterCommand::Frame(
            V6OutboundCoreFrame::Health {
                request_id: "busy".to_owned(),
            },
            Flush::Lifecycle("busy".to_owned()),
        ))
        .unwrap();
    let (reply, mut receiver) = oneshot::channel();
    let mut in_flight = None;
    let mut awaiting_lifecycle_flush = HashSet::new();
    let result = dispatch_lifecycle(
        &mut in_flight,
        &mut awaiting_lifecycle_flush,
        &writer,
        V6OutboundCoreFrame::Health {
            request_id: "7".to_owned(),
        },
        Expected::Health,
        reply,
    );
    assert!(matches!(result, Err(FatalReason::Backpressure)));
    assert!(in_flight.is_none());
    assert!(matches!(
        receiver.try_recv(),
        Ok(Err(ExternalError::Backpressure))
    ));
}

#[tokio::test]
async fn in_flight_deadline_fires_without_event_activity() {
    let (reply, _receiver) = oneshot::channel();
    let in_flight = Some(Pending {
        request_id: "1".to_owned(),
        expected: Expected::Health,
        deadline: Some(Instant::now()),
        reply,
    });
    tokio::time::timeout(
        Duration::from_millis(20),
        sleep_until_in_flight_deadline(&in_flight),
    )
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn supervisor_kills_descendant_after_leader_exit() {
    use std::fs;

    struct NoopExecutor;
    impl V6TelegramExecutor for NoopExecutor {
        fn execute<'a>(
            &'a self,
            _context: V6ExecutionContext,
            _method: v6_registry::V6Method,
            _params: Box<serde_json::value::RawValue>,
        ) -> super::super::v6_executor::V6ExecutorFuture<'a> {
            Box::pin(async { Err(V6ExecutorError::Transport) })
        }
    }

    let root = std::env::temp_dir().join(format!(
        "lavis-v6-descendant-{}-{}",
        std::process::id(),
        protocol::request_id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let entrypoint = root.join("run");
    ensure_test_state_base();
    write_v6_fixture(
        &entrypoint,
        "#!/usr/bin/env python3\nimport subprocess, sys\np = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\nwith open('child.pid', 'w') as f:\n    f.write(str(p.pid))\nsys.exit(0)\n",
    );

    let mut module = descriptor();
    module.id = format!("v6desc{}", std::process::id());
    module.module_dir = root.clone();
    module.entrypoint = entrypoint;
    let process = V6Process::start(module, Arc::new(NoopExecutor), 1)
        .await
        .unwrap();

    let pid_path = root.join("child.pid");
    for _ in 0..100 {
        if pid_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let descendant: i32 = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    for _ in 0..100 {
        if process.status() == super::super::process::ProcessStatus::Crashed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        process.status(),
        super::super::process::ProcessStatus::Crashed
    );

    fn descendant_is_live(pid: i32) -> bool {
        let proc_stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        match proc_stat {
            Ok(stat) => {
                // `/proc/<pid>/stat` is `pid (comm) state ...`; comm may
                // contain spaces or parentheses, so find the final `) `.
                // A zombie has already been killed and cannot execute or
                // retain module resources; in a PID namespace its init is
                // responsible for the eventual wait/reap.
                let Some(after_comm) = stat.rsplit_once(") ").map(|(_, tail)| tail) else {
                    return true;
                };
                !after_comm.starts_with('Z')
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => true,
        }
    }

    let mut alive = true;
    for _ in 0..200 {
        alive = descendant_is_live(descendant);
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if alive {
        unsafe {
            let _ = libc::kill(descendant, libc::SIGKILL);
        }
    }
    let _ = fs::remove_dir_all(&root);
    assert!(
        !alive,
        "live descendant process survived v6 supervisor cleanup"
    );
}
