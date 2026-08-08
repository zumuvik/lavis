# External Module API: schema 2 and 3 (alpha)

External modules are self-contained programs launched as child processes and controlled through a newline-delimited JSON protocol. A module may be implemented in any language as long as its entrypoint is directly executable and follows the selected protocol version.

> **Status:** alpha. Manifest schema, protocol messages and runtime behavior may change without compatibility guarantees.

## Architecture

```text
┌──────────────┐   JSON Lines over stdin/stdout   ┌──────────────────┐
│    Lavis     │ ◄──────────────────────────────► │  Module Process  │
│    core      │                                  │  any language    │
└──────────────┘                                  └──────────────────┘
```

Each enabled external module is a separate OS process started during Lavis startup. Installing a module does not enable or start it.

## Module directory layout

```text
my-module/
├── module.json
└── bin/
    └── my-module
```

Requirements:

- `module.json` is a regular file in the module root;
- `entrypoint` is a relative path below the module root;
- the entrypoint is a regular executable file, not a symlink;
- the module directory, manifest and entrypoint must not be group- or world-writable;
- module and command identifiers use lowercase ASCII letters, digits and `-`, begin with a lowercase ASCII letter and are at most 32 bytes.

## Manifest

Lavis accepts `schema_version` **2** and **3**. The schema version is also the JSON protocol version used for the process.

### Schema 2 example

```json
{
  "schema_version": 2,
  "id": "my-module",
  "name": "My Module",
  "version": "1.0.0",
  "author": "Your Name",
  "entrypoint": "bin/my-module",
  "capabilities": [],
  "commands": [
    {
      "name": "greet",
      "summary_ru": "Приветствие",
      "description_ru": "Возвращает приветствие с переданным именем.",
      "usage": "[имя]",
      "examples": ["Alice", "Мир"]
    }
  ]
}
```

### Schema 3 example

```json
{
  "schema_version": 3,
  "id": "autoreact",
  "name": "AutoReact",
  "version": "1.0.0",
  "author": "Your Name",
  "entrypoint": "bin/autoreact",
  "capabilities": ["message.read", "message.react"],
  "default_command": "manage",
  "subscriptions": ["message.created"],
  "actions": ["message.react"],
  "commands": [
    {
      "name": "manage",
      "summary_ru": "Управление",
      "description_ru": "Настраивает поведение модуля.",
      "usage": "[on|off]",
      "examples": ["on", "off"]
    }
  ]
}
```

Unknown manifest fields are rejected.

### Common fields

| Field | Required | Contract |
| --- | --- | --- |
| `schema_version` | yes | `2` or `3`; also selects the process protocol version. |
| `id` | yes | Unique lowercase ASCII identifier, 1–32 bytes. Must match the installed directory name. |
| `name` | yes | Single-line display name, 1–64 Unicode characters. |
| `version` | yes | Single-line version string, 1–32 Unicode characters. |
| `author` | yes | Single-line author or handle, 1–128 Unicode characters. |
| `entrypoint` | yes | Safe relative path to a regular executable below the module root. |
| `capabilities` | no | Unique descriptive capability strings. |
| `commands` | yes | 1–32 unique command descriptors. |

Control characters and Unicode bidi controls are rejected from display fields. `module.json` is limited to 64 KiB.

### Command descriptors

| Field | Required | Contract |
| --- | --- | --- |
| `name` | yes | Lowercase ASCII command identifier, 1–32 bytes. |
| `summary_ru` | yes | Single line, 1–120 Unicode characters. |
| `description_ru` | yes | 1–2000 Unicode characters; ordinary newlines are allowed. |
| `usage` | yes | Argument syntax only, without the command name; single line, 1–256 characters. |
| `examples` | no | Up to 16 argument-only examples, each at most 256 characters. |

Commands are invoked as:

```text
,<module-id>.<command-name> [arguments]
```

Schema 3 may define `default_command`. When the module is active, the default command is also available as:

```text
,<module-id> [arguments]
```

### Capabilities

Capabilities are descriptive metadata. They are validated for consistency but are **not** a sandbox or OS permission system.

Supported values:

- `host_information`
- `network`
- `persistent_state_read`
- `persistent_state_write`
- `message.read`
- `message.react`

`message.created` requires `message.read`. The `message.react` action requires `message.react`.

### Schema 3-only fields

Schema 2 rejects these fields when they are set:

| Field | Supported values | Meaning |
| --- | --- | --- |
| `default_command` | name of a declared command | Enables the short `,<module-id>` invocation. |
| `subscriptions` | `message.created` | Delivers new-message events to the active module. |
| `actions` | `message.react` | Allows one scoped reaction action in an event response. |

Values must be unique.

## JSON-line protocol

Every message is one UTF-8 JSON object followed by a newline. Modules must flush stdout after each reply. Lines larger than 64 KiB are rejected.

Every request contains a decimal `request_id`. A reply must echo exactly the same ID.

### Core → module

#### Initialize

```json
{"protocol_version":2,"type":"initialize","request_id":"1","module_id":"my-module"}
```

Reply:

```json
{"protocol_version":2,"type":"initialized","request_id":"1","module_id":"my-module"}
```

#### Execute

Schema 2:

```json
{"protocol_version":2,"type":"execute","request_id":"2","command":"greet","arguments":"Alice"}
```

Schema 3 may include Telegram custom-emoji entities projected only from the command arguments:

```json
{
  "protocol_version": 3,
  "type": "execute",
  "request_id": "2",
  "command": "manage",
  "arguments": "hello",
  "context": {
    "argument_entities": [
      {
        "type": "custom_emoji",
        "offset_utf16": 0,
        "length_utf16": 2,
        "document_id": "5456140674028019486"
      }
    ]
  }
}
```

Success reply:

```json
{"protocol_version":2,"type":"result","request_id":"2","text":"Hello, Alice!"}
```

Module error reply:

```json
{"protocol_version":2,"type":"error","request_id":"2","code":"invalid_input","message":"Name is required"}
```

Result text is limited to 32 KiB. Error code and message are each limited to 256 characters.

#### Health

```json
{"protocol_version":2,"type":"health","request_id":"3"}
```

```json
{"protocol_version":2,"type":"health","request_id":"3"}
```

#### Shutdown

```json
{"protocol_version":2,"type":"shutdown","request_id":"4"}
```

The module should finish promptly after receiving shutdown.

#### Log

A module may emit a bounded structured log message:

```json
{"protocol_version":2,"type":"log","request_id":"2","level":"info","message":"request handled"}
```

Module stderr is drained continuously and the first 16 KiB are kept. When the module crashes, fails the protocol, times out or fails handshake, the captured stderr is included in the `external_module_crashed` event at error level; invalid UTF-8 is converted lossy. A normal shutdown does not create a crash event, and a protocol-valid application `error` reply during execute is not a crash either: it does not emit `external_module_crashed`, does not terminate the module process, and the module stays ready for subsequent requests. Modules must not write secrets to stderr.

## Schema 3 events and actions

An active schema 3 module that declares `message.created` and `message.read` receives:

```json
{
  "protocol_version": 3,
  "type": "event",
  "request_id": "5",
  "event": "message.created",
  "payload": {
    "event_id": "...",
    "message_ref": "opaque-64-hex-reference",
    "text": "message text",
    "outgoing": true,
    "entities": []
  }
}
```

`message_ref` is opaque and scoped to the request. It does not expose a Telegram peer or message ID.

A module may reply with at most one action:

```json
{
  "protocol_version": 3,
  "type": "event_result",
  "request_id": "5",
  "actions": [
    {
      "type": "message.react",
      "message_ref": "opaque-64-hex-reference",
      "reaction": {"type":"emoji","emoji":"👍"}
    }
  ]
}
```

Custom emoji reaction:

```json
{
  "type": "message.react",
  "message_ref": "opaque-64-hex-reference",
  "reaction": {
    "type": "custom_emoji",
    "document_id": "5456140674028019486"
  }
}
```

The action is rejected unless all of the following match: module ID, request ID, opaque message reference, declared action and required capability. Custom emoji document IDs must be decimal `i64` values; ordinary emoji strings are bounded and reject control/bidi characters.

## Process environment

The child environment is cleared. Lavis supplies only:

- `NO_COLOR=1`
- `CLICOLOR=0`
- `CLICOLOR_FORCE=0`
- `TERM=dumb`
- a fixed minimal `PATH`

The host `PATH`, Telegram credentials and arbitrary host environment variables are not inherited. Lavis does not invoke a shell; the manifest entrypoint is executed directly.

## Installation and activation

A module may be placed manually under `$XDG_DATA_HOME/lavis/modules/<id>/`, or installed from a checked `.lmod` attachment using Telegram commands:

```text
,lm install
,lm confirm XXXX-XXXX-XXXX-XXXX
```

Installation validates the manifest from the final path and never overwrites an existing module directory. The installed module remains disabled. Enable it with:

```bash
lavis modules enable <id>
```

Then restart Lavis. See [External Modules](external-modules.md) and [Packaging `.lmod`](lmod-packaging.md).

## Security model

External modules are arbitrary executable code running with the OS permissions of the Lavis user. No capability enforcement, seccomp, container, WASM runtime or other system sandbox is applied. Inspection verifies archive structure and metadata; it does not establish trust, provenance or absence of malicious behavior.

Only enable code you trust.

## Current limitations

- alpha API and protocol with no compatibility guarantee;
- no hot enable/disable or hot reload;
- no module-to-module communication;
- no update, replacement or uninstall command;
- no remote repository or package registry installation;
- no signatures, trust store or dependency builder;
- no system sandbox;
- `.lmod` installation supports only a same-message Telegram document in Saved Messages;
- installed modules must be enabled through the local CLI and start on the next Lavis launch.
