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

`raw.invoke` is not a way to retrieve the Telegram session or auth key. Lavis
does not transmit session data, auth keys, or API credentials through Module
API v6 IPC.

## Framing

All frames are one UTF-8 JSON object followed by `\n` on stdout/stdin. V6 uses
`protocol_version: 6`. A line may not exceed 64 KiB (`MAX_LINE_BYTES`); longer
lines are a protocol violation.

## Alpha wire-contract fixture

The independently consumable frozen artifact is
`protocol/v6/alpha-contract.json`, with its schema at
`protocol/v6/alpha-contract.schema.json`. `schema_version` governs the artifact
format; `contract_revision` governs compatible v6-alpha clarifications; and an
incompatible wire change requires a new `protocol_version`. The artifact covers
both inbound and outbound JSON transcripts, including malformed frames and
closing/timeout/shutdown semantics.

V6 keeps distinct lifecycle and RPC deadlines even though alpha currently sets
both to five seconds: lifecycle timeout begins after its frame is written and
flushed, while RPC timeout applies to executor work. V2-v5 peers must never be
sent v6 frames; compatible clarifications require a `contract_revision` bump.

### Contract revision 4: companion identity

A module whose manifest declares `contract_revision: 4` receives an additional
optional object in the `execute` context:

```json
{"context": {"companion": {"chat_id": -1001234567890, "access_hash": 123456789012345}}}
```

The object carries the setup-created companion group identity and is present
only when the host has a configured companion. Modules must not require it and
must ignore unknown context fields. Contract revision 3 modules keep the exact
revision-3 wire shape: the field is omitted for them.

## Conformance runner

`lavis-v6-conformance [--profile base|full] <executable> [arguments...]` embeds
the frozen alpha contract, launches the supplied module with piped stdin/stdout,
drives the mandatory lifecycle transcript initialize, execute, event, health,
then shutdown, and validates every received frame through Lavis' production v6
parser. During lifecycle waits it deterministically services module-initiated
`telegram.invoke` calls with contract-shaped results. Every lifecycle response
must carry the matching request ID, and the initialized response must echo
module ID `conformance`. Each response read has a five-second deadline;
malformed or uncorrelated frames fail conformance.

The default `base` profile validates protocol and lifecycle conformance only: a
correct v6 module passes regardless of which Telegram methods it calls, and a
module that never calls `raw.invoke` passes, because `telegram.raw` is an
opt-in high-risk capability. The `full` RPC capability profile additionally
requires the module to exercise at least one curated helper and at least one
`raw.invoke` call during the transcript. After the shutdown frame the runner
waits at most five seconds for the module to exit; a module that does not exit
within the deadline fails conformance.

Lifecycle frames use decimal `request_id` values of 1-64 ASCII digits. A
lifecycle request has exactly one matching response; mismatched or duplicate
request IDs are a protocol violation. Parentless Telegram calls use independent
`call_id` values consisting of ASCII alphanumeric characters, `_`, or `-`, up
to 64 bytes.

### Lifecycle

Lavis drives lifecycle requests strictly in order. At most one lifecycle request
is sent and awaiting a response at a time. Additional lifecycle requests wait
in a bounded queue (`V6_MAX_PENDING`); their response timeout starts only after
the lifecycle frame has been written and flushed to the module's stdin, not
while it is queued or being submitted to the writer. Parentless
`telegram.invoke` calls remain independent and may be processed while a
lifecycle response is pending.

| Direction | Frame | Response |
| --- | --- | --- |
| Lavis → module | `{"type":"initialize","request_id":"1","module_id":"<id>"}` | `{"type":"initialized","request_id":"1","module_id":"<id>"}` |
| Lavis → module | `{"type":"execute","request_id":"2","command":"name","arguments":"...","context":{"argument_entities":[],"companion":{"chat_id":-1001234567890,"access_hash":123456789012345}}}` | `{"type":"result","request_id":"2","text":"..."}` |
| Lavis → module | `{"type":"event","request_id":"3","event":"message.created","payload":{"event_id":"...","message_ref":"...","message_key":"...","text":"...","outgoing":false,"entities":[],"peer_id":123}}` | `{"type":"event_result","request_id":"3","actions":[]}` |
| Lavis → module | `{"type":"health","request_id":"4"}` | `{"type":"health","request_id":"4"}` |
| Lavis → module | `{"type":"shutdown","request_id":"5"}` | no response; the module exits |

The `initialized` response must repeat the exact `module_id` from the
`initialize` request; a mismatch is fatal. An `event_result` requires an
`actions` array (bounded to one action, with at most three reactions per
action).

The module may emit `telegram.invoke` frames at any time, including between
lifecycle requests.

`log` is lifecycle-correlated: it contains `request_id`, `level`, and
`message`, and its request ID must identify the currently pending lifecycle
request. It may be emitted while that lifecycle request is in flight, but is
not a replacement for its response. `error` is likewise permitted only as the
response to the active lifecycle request, with the matching `request_id`.
Lifecycle responses use the matching request ID and the response type permitted
for that request. `telegram.invoke` is permitted while lifecycle requests are
pending and is correlated independently by `call_id`. If a log races
cancellation of the active lifecycle request during closing, Lavis drains that
log without crashing the module runtime.

The event `event` field is the event kind (the example uses exactly
`message.created`). Event data is nested under `payload`; `payload.peer_id` is
optional and is included only when the event has a peer.

### Parentless Telegram calls

A module starts a Telegram call with:

```json
{
  "protocol_version": 6,
  "type": "telegram.invoke",
  "call_id": "rpc-1",
  "method": "contacts.getContacts",
  "params": {"hash": "0"}
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
    "message": "Telegram RPC request failed",
    "code": null,
    "name": null,
    "retry_after_seconds": null
  }
}
```

The `error` object always carries `kind` and `message`, plus the optional
structured metadata fields `code`, `name`, and `retry_after_seconds`. When a
field is not applicable it is serialized as `null`; when applicable it carries
the Telegram RPC error code, the Telegram RPC error name, and the retry delay
for rate-limited calls respectively. For example, a rate-limited call is
reported as:

```json
{
  "protocol_version": 6,
  "type": "telegram.result",
  "call_id": "rpc-1",
  "ok": false,
  "error": {
    "kind": "rpc",
    "message": "Telegram RPC request failed",
    "code": 420,
    "name": "FLOOD_WAIT",
    "retry_after_seconds": 7
  }
}
```

RPC errors are not retried by the module protocol. Modules must not infer a
retry policy from error text; `retry_after_seconds` is advisory and may be
`null`.

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
page `limit` values outside `1..=100` are rejected. Curated limits are strict
validation limits; Lavis does not silently clamp invalid input. The same
reject-on-limit rule applies to curated helper collection and result bounds.

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

An optional datacenter can be selected explicitly. `dc_id` is a bounded integer
transport selector; this contract does not promise validation against a finite
list of known datacenters:

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
- Lavis does not transmit session bytes, auth keys, API credentials, or sender
  handles through the Module API v6 IPC protocol;
- RPC failures returned to the module are sanitized.

Per-module fairness/concurrency limits (`V6_MAX_ACTIVE_RPCS` in-flight calls
per module) are enforced by the runtime in addition to the global v6 RPC
semaphore.

## Peer and message handles

Invocations receive scoped handles instead of raw chat identifiers.

- The peer handle is issued for the invocation's real `PeerId`. Repeated
  invocations from the same chat reuse the same peer handle within one process
  generation; a handle issued for one chat is never observable from an
  invocation of another chat.
- The current-message handle carries an editable authority flag: it is
  editable only when the triggering message was authored by the signed-in
  user. Handles for other messages (for example the replied-to message) are
  never editable.
- A handle release requested while a host call is active is deferred until
  that host call completes, and the deferred releases are drained then.
- Host calls are restricted to `message.edit` and `message.sendBot`. A host
  frame carrying any other method, or a host call beyond the active-call
  capacity, terminates the process as a fatal protocol or backpressure
  violation (retained in diagnostics). Over-capacity Telegram RPC calls, in
  contrast, still receive a soft `capacity` error result.

### Companion-bot posts: `message.sendBot`

A module whose manifest declares the `message.send_bot` capability may ask the
host to post a plain-text message as the companion bot:

```json
{"type":"host.invoke","call_id":"rpc-9","method":"message.sendBot",
 "params":{"chat_id":-1001234567890,"message_thread_id":7,"text":"…"}}
```

- `chat_id` is required and nonzero; `message_thread_id` is optional and must
  be positive; `text` is required, must not contain NUL, and is capped at
  4096 UTF-16 units like `message.edit`.
- The host resolves the destination through the Bot API using its own
  companion-bot credentials. Modules never receive or transmit tokens, and
  delivery succeeds only in chats where the companion bot can post.
- Results are `null` on success or a sanitized host error (`capability
  denied`, `bot send rejected`, `bot send timeout`, `bot send unavailable`);
  the host never forwards HTTP or token material into module-visible errors.

## Resource limits

V6 remains a bounded protocol.

### JSON guards

Inbound module JSON and curated-helper parameters/results are validated
strictly with these guards. Typed host-generated lifecycle frames are bounded
by the serialized line limit as well; their fields are generated by Lavis and
are not passed through the generic inbound untrusted-JSON tree guards.

| Limit | Value |
| --- | --- |
| Maximum JSON nesting depth | 8 (`V6_MAX_JSON_DEPTH`) |
| Maximum string length | 8 KiB (`V6_MAX_JSON_STRING_BYTES`) |
| Maximum array/object items | 64 (`V6_MAX_JSON_COLLECTION_ITEMS`) |
| Maximum line | 64 KiB (`MAX_LINE_BYTES`) |
| Maximum curated result | 32 KiB (`MAX_RESULT_BYTES`) |
| Maximum error message | 256 chars (`MAX_ERROR_MESSAGE_CHARS`) |
| Maximum log message | 1024 chars (`MAX_LOG_MESSAGE_CHARS`) |

The 256-character bound applies independently to inbound `error.code` and
`error.message`; the 1024-character bound applies independently to inbound
`log.level` and `log.message`. Values over these bounds are rejected, not
truncated.

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
| Maximum queued lifecycle requests | 8 (`V6_MAX_PENDING`) |
| Maximum active Telegram calls | 8 (`V6_MAX_ACTIVE_RPCS`) |
| Global RPC concurrency (all modules) | 8 (`V6_GLOBAL_CONCURRENCY`) |
| Lifecycle request timeout | 5 s (`V6_LIFECYCLE_TIMEOUT`) |
| RPC execution timeout | 5 s (`V6_RPC_TIMEOUT`) |
| Write timeout | 1 s (`V6_WRITE_TIMEOUT`) |
| Shutdown grace | 1 s (`V6_SHUTDOWN_TIMEOUT`) |

A full waiting lifecycle queue (`V6_MAX_PENDING`) rejects new requests with a
backpressure category. A full RPC queue rejects an excess `telegram.invoke`
with a `capacity` error. These are distinct from a protocol crash: transient
backpressure must not be indistinguishable from a writer failure.

## Shutdown behavior

Graceful shutdown proceeds as follows:

1. Lavis stops accepting new lifecycle requests and marks the module as
   closing.
2. A `shutdown` frame is flushed to the module's stdin.
3. Lavis establishes a hard shutdown barrier. After the barrier, no new
   module-directed frame is written.
4. A `telegram.invoke` received after the barrier is discarded without a
   `telegram.result`; an RPC completion that arrives late is also discarded.
   No late result is written behind the `shutdown` frame. This prevents a race
   with module exit and a closed pipe.
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

The exact module-to-host error fields are `protocol_version`, `type: "error"`,
`request_id`, `code`, and `message`. The exact module log fields are
`protocol_version`, `type: "log"`, `request_id`, `level`, and `message`.
Lifecycle responses and logs correlate with `request_id`; Telegram requests
and results correlate with `call_id`.

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
running index (startup failure, crash cleanup), and are surfaced through
`lm logs <id>` and `lm doctor`. Lavis does not intentionally add credentials,
session data, or raw TL bodies to diagnostics. Module-controlled stderr is
untrusted text and may contain sensitive content; bounding and retaining it is
not an absolute secrecy guarantee.

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
