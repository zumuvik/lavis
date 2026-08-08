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
`protocol_version: 6`.

Lifecycle frames use decimal `request_id` values. Parentless Telegram calls use
independent `call_id` values consisting of ASCII alphanumeric characters,
`_`, or `-`, up to 64 bytes.

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

Lavis answers:

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

V6 remains a bounded protocol. The current raw TL adapter limits one request or
response body to 40 KiB so Base64 plus JSON framing stays inside the existing
64 KiB line boundary. Operations with naturally large payloads must split work
across multiple Telegram RPCs. Raising or streaming this transport bound is a
protocol-transport change, not a Telegram method-surface change.

## Compatibility

Protocols v2-v5 do not understand `telegram.invoke`, `telegram.result`,
`telegram.raw`, `telegram_methods`, or `raw.invoke`. Their existing wire format
and manifest behavior remain unchanged.
