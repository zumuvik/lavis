# Module API v6

API v6 keeps the module process persistent and allows parentless Telegram RPC
calls while lifecycle requests are in flight.

## Completeness rule

A v6 module must be able to use a valid Telegram RPC that Lavis has no
purpose-built adapter for without modifying or rebuilding Lavis.

The Telegram surface therefore has two layers:

1. **Curated helpers** such as `messages.getHistory`. Lavis validates typed JSON
   parameters and returns deliberately shaped JSON results.
2. **`raw.invoke`**, the stable escape hatch. The module serializes a Telegram
   TL request itself; Lavis transports the opaque bytes through the already
   authenticated sender pool and returns the opaque TL response bytes.

`raw.invoke` is not a way to retrieve the Telegram session or auth key. Those
never cross the module boundary.

## Framing

All frames are one UTF-8 JSON object followed by `\n` on stdout/stdin. V6 uses
`protocol_version: 6`. A line may not exceed 64 KiB (`MAX_LINE_BYTES`); longer
lines are a protocol violation.

Lifecycle frames use decimal `request_id` values of 1-64 ASCII digits. A
lifecycle request has exactly one matching response; mismatched or duplicate
request IDs are a protocol violation. Parentless Telegram calls use independent
`call_id` values consisting of ASCII alphanumeric characters, `_`, or `-`, up
to 64 bytes.

### Lifecycle

Lavis drives the module strictly in order. Each request below must be answered
with the matching response before Lavis sends the next one.

| Direction | Frame | Response |
| --- | --- | --- |
| Lavis → module | `{"type":"initialize","request_id":"1","module_id":"<id>"}` | `{"type":"initialized","request_id":"1","module_id":"<id>"}` |
| Lavis → module | `{"type":"execute","request_id":"2","command":"name","arguments":"...","argument_entities":[]}` | `{"type":"result","request_id":"2","text":"..."}` |
| Lavis → module | `{"type":"event","request_id":"3","text":"...","entities":[]}` | `{"type":"event_result","request_id":"3","actions":[]}` |
| Lavis → module | `{"type":"health","request_id":"4"}` | `{"type":"health","request_id":"4"}` |
| Lavis → module | `{"type":"shutdown","request_id":"5"}` | no response; the module exits |

The `initialized` response must repeat the exact `module_id` from the
`initialize` request; a mismatch is fatal. An `event_result` requires an
`actions` array (bounded to one action, with at most three reactions per
action).

The module may emit `telegram.invoke` frames at any time, including between
lifecycle requests.

### Parentless Telegram calls

A module starts a Telegram call with:

```json
{
  "protocol_version": 6,
  "type": "telegram.invoke",
  "call_id": "rpc-1",
  "method": "messages.getHistory",
  "params": {}
}
```

Lavis answers with the same `call_id`:

```json
{
  "protocol_version": 6,
  "type": "telegram.result",
  "call_id": "rpc-1",
  "ok": true,
  "result": {}
}
```

or a sanitized error:

```json
{
  "protocol_version": 6,
  "type": "telegram.result",
  "call_id": "rpc-1",
  "ok": false,
  "error": {
    "kind": "rpc",
    "message": "Telegram RPC request failed"
  }
}
```

Call IDs must be unique while a call is active. Duplicate active call IDs are a
protocol violation.

## Curated helpers

Curated helpers remain useful for common operations because they can:

- validate a small stable input schema;
- perform safe peer resolution;
- redact access hashes and internal Telegram objects;
- return small, version-stable result objects;
- provide operation-specific limits.

Their presence is optional for API completeness. New Telegram methods do not
need to be added to Lavis merely so a module can use them.

Curated helpers have strict typed parameter decoding: unknown fields are
rejected, peer values are limited to the authenticated user's own `self`, and
page `limit` values are clamped to `1..=100`.

## Raw Telegram invocation

A module requesting raw access declares:

```json
{
  "schema_version": 6,
  "capabilities": ["telegram.raw"],
  "telegram_methods": ["raw.invoke"]
}
```

The install fingerprint covers both the capability and method grant.

The module then sends:

```json
{
  "protocol_version": 6,
  "type": "telegram.invoke",
  "call_id": "raw-1",
  "method": "raw.invoke",
  "params": {
    "body_base64_chunks": ["eFY0EgEAAAA="]
  }
}
```

`body_base64_chunks` concatenates to standard padded Base64. Decoded bytes are
the complete serialized TL function body, including its constructor ID. The
body must be non-empty, 4-byte aligned, and fit the bounded v6 IPC transport.

An optional datacenter can be selected explicitly:

```json
{
  "dc_id": 4,
  "body_base64_chunks": ["..."]
}
```

If `dc_id` is omitted, Lavis uses the session home datacenter.

A successful raw result is:

```json
{
  "kind": "raw_tl",
  "dc_id": 4,
  "body_base64_chunks": ["..."]
}
```

The module is responsible for deserializing the returned TL object and for
choosing a Telegram TL layer compatible with the request it serialized.

### Raw authority

Granting `telegram.raw` plus `raw.invoke` gives the module the ability to issue
arbitrary Telegram RPC request bodies as the signed-in account, subject to
Telegram server authorization and Lavis resource limits. It is therefore a
high-risk install-time capability.

Lavis must still enforce these boundaries:

- raw calls share the global v6 RPC concurrency limit and bounded module queues;
- raw calls share RPC timeouts and shutdown cancellation;
- request and response bodies are bounded;
- raw bodies are never logged or persisted;
- session bytes, auth keys, API credentials, and sender handles are never
  exposed to the module;
- RPC failures returned to the module are sanitized.

Per-module fairness/concurrency limits are separate follow-up work; the current
v6 contract does not claim they already exist.

## Resource limits

V6 remains a bounded protocol.

### JSON guards

Applied to every v6 frame and to helper parameters/results:

| Limit | Value |
| --- | --- |
| Maximum JSON nesting depth | 8 (`V6_MAX_JSON_DEPTH`) |
| Maximum string length | 8 KiB (`V6_MAX_JSON_STRING_BYTES`) |
| Maximum array/object items | 64 (`V6_MAX_JSON_COLLECTION_ITEMS`) |
| Maximum line | 64 KiB (`MAX_LINE_BYTES`) |
| Maximum curated result | 32 KiB (`MAX_RESULT_BYTES`) |
| Maximum error message | 256 chars (`MAX_ERROR_MESSAGE_CHARS`) |
| Maximum log message | 1024 chars (`MAX_LOG_MESSAGE_CHARS`) |

### Raw TL bodies

| Limit | Value |
| --- | --- |
| Maximum raw TL body | 40 KiB (`MAX_RAW_TL_BODY_BYTES`) |
| Maximum Base64 chunk | 7168 chars (`RAW_BASE64_CHUNK_CHARS`) |
| Alignment | 4-byte aligned, non-empty |

40 KiB of body plus Base64 and JSON framing stays inside the 64 KiB line
boundary. Operations with naturally large payloads must split work across
multiple Telegram RPCs. Raising or streaming this transport bound is a
protocol-transport change, not a Telegram method-surface change.

### Queues, concurrency, and timeouts

| Limit | Value |
| --- | --- |
| Control queue | 4 (`V6_CONTROL_QUEUE`) |
| Reader queue | 8 (`V6_READER_QUEUE`) |
| Writer queue | 8 (`V6_WRITER_QUEUE`) |
| RPC event queue | 8 (`V6_RPC_QUEUE`) |
| Maximum pending lifecycle requests | 8 (`V6_MAX_PENDING`) |
| Maximum active Telegram calls | 8 (`V6_MAX_ACTIVE_RPCS`) |
| Global RPC concurrency (all modules) | 8 (`V6_GLOBAL_CONCURRENCY`) |
| Lifecycle request timeout | 5 s (`V6_LIFECYCLE_TIMEOUT`) |
| RPC execution timeout | 5 s (`V6_RPC_TIMEOUT`) |
| Write timeout | 1 s (`V6_WRITE_TIMEOUT`) |
| Shutdown grace | 1 s (`V6_SHUTDOWN_TIMEOUT`) |

A full lifecycle queue (`V6_MAX_PENDING`) rejects new requests with a
backpressure category. A full RPC queue rejects an excess `telegram.invoke`
with a `capacity` error. These are distinct from a protocol crash: transient
backpressure must not be indistinguishable from a writer failure.

## Shutdown behavior

Graceful shutdown proceeds as follows:

1. Lavis stops accepting new lifecycle requests and marks the module as
   closing.
2. A `shutdown` frame is flushed to the module's stdin.
3. New `telegram.invoke` frames arriving after shutdown begins are rejected
   with a sanitized `shutdown` error.
4. In-flight Telegram calls are cancelled at shutdown: a completion that lands
   after shutdown began is discarded and never written behind the `shutdown`
   frame. This is a hard barrier — a late write would race the module's exit
   and a closed pipe into a false writer failure.
5. The module exits; exit status zero completes the shutdown. A non-zero exit
   during shutdown is recorded as a crash with the exit code and retained
   stderr.
6. If the module does not exit within the shutdown grace period, Lavis kills
   the whole process group (leader and descendants).

`terminate()` is a real force-termination path: it cancels pending lifecycle
requests, aborts RPC workers, and kills the process group; it does not retry a
graceful shutdown.

## Failure diagnostics

Every terminal failure records a bounded diagnostic with: module ID and
protocol version; lifecycle stage (`spawn`, `initialize`, `execute`, `event`,
`health`, `rpc`, `shutdown`); request ID when available; a stable error
category; exit status or signal; bounded UTF-8-lossy stderr; truncation flag;
timestamp; and a restart generation.

Stable error categories include:

| Category | Meaning |
| --- | --- |
| `unavailable` | child or transport unavailable (includes unexpected exit) |
| `protocol_decode` | malformed frame, wrong request/module ID, JSON guard breach |
| `line_too_large` | line exceeded `MAX_LINE_BYTES` |
| `wrong_request_id` | lifecycle response did not match the pending request |
| `wrong_module_id` | `initialized` echoed a different module ID |
| `execution_timeout` | lifecycle request exceeded `V6_LIFECYCLE_TIMEOUT` |
| `shutdown_timeout` | module did not exit within the shutdown grace |
| `backpressure` | lifecycle queue full |
| `writer_unavailable` | module stdin closed while a frame was pending |

Diagnostics are retained by the runtime even after the process leaves the
running index (startup failure, crash cleanup), are surfaced through
`lm logs <id>` and `lm doctor`, and never include credentials, session data,
raw TL bodies, or unrestricted payloads.

## Capability and typed-helper grant rules

- Curated helpers require an explicit entry in `telegram_methods`; they do not
  require `telegram.raw`.
- `raw.invoke` requires the explicit high-risk `telegram.raw` capability.
- Unlisted methods are rejected before the executor runs, with a
  `capability`/`validation` error.
- The install plan and fingerprint make both `telegram.raw` and `raw.invoke`
  visible. Granting `raw.invoke` means trusting the module to act with the
  Telegram authority of the signed-in account; it is not a sandbox boundary.

## Compatibility

Protocols v2-v5 do not understand `telegram.invoke`, `telegram.result`,
`telegram.raw`, `telegram_methods`, or `raw.invoke`. Their existing wire format
and manifest behavior remain unchanged.
