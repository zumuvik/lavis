# Module API v4: edited messages and reaction sets

Module API v4 extends schema/protocol v3 without changing v2 or v3 wire behavior.
It is intended for external modules that must reconcile reactions after a message
is edited.

## Manifest

A v4 module uses `schema_version: 4` and may subscribe to both message events:

```json
{
  "schema_version": 4,
  "id": "autoreact",
  "name": "AutoReact",
  "version": "1.0.0",
  "author": "Example",
  "entrypoint": "autoreact",
  "capabilities": ["message.read", "message.react"],
  "subscriptions": ["message.created", "message.edited"],
  "actions": ["message.react"],
  "commands": [
    {
      "name": "manage",
      "summary_ru": "Управление",
      "description_ru": "Настраивает реакции.",
      "usage": "[аргументы]",
      "examples": []
    }
  ]
}
```

`message.created` and `message.edited` require `message.read`. The
`message.react` action requires `message.react`. Schema v3 still accepts only
`message.created`; schema v2 accepts neither subscriptions nor actions.

## Event payload

```json
{
  "protocol_version": 4,
  "type": "event",
  "request_id": "17",
  "event": "message.edited",
  "payload": {
    "event_id": "18",
    "message_ref": "request-scoped-opaque-reference",
    "message_key": "stable-module-scoped-opaque-key",
    "text": "edited text",
    "outgoing": false,
    "entities": []
  }
}
```

`message_ref` is valid only for the current request and must be copied into an
action. `message_key` is stable for the same Telegram message and module across
`message.created` and `message.edited`; modules may use it as a reconciliation
key. Neither value exposes the Telegram peer or message ID.

New Lavis command/setup messages are not projected to modules. If a previously
projected message is edited into protected command/setup content, v4 subscribers
receive a redacted `message.edited` event with empty `text` and `entities`. This
allows modules to remove prior actions without receiving the protected text.

## Atomic reaction set

A v4 event response may contain at most one action. `message.react` replaces the
complete reaction set of the Lavis account for that message with zero to three
unique reactions:

```json
{
  "protocol_version": 4,
  "type": "event_result",
  "request_id": "17",
  "actions": [
    {
      "type": "message.react",
      "message_ref": "request-scoped-opaque-reference",
      "reactions": [
        {"type": "emoji", "emoji": "👍"},
        {"type": "custom_emoji", "document_id": "5456140674028019486"}
      ]
    }
  ]
}
```

An empty `reactions` array removes the account reaction from the message:

```json
{
  "protocol_version": 4,
  "type": "event_result",
  "request_id": "17",
  "actions": [
    {
      "type": "message.react",
      "message_ref": "request-scoped-opaque-reference",
      "reactions": []
    }
  ]
}
```

For compatibility, protocol v3 keeps the singular `reaction` field and exactly
one reaction per action.

## Reference module

`modules/gaf/` is a v4 external module that stores word triggers, applies up to
three ordinary or Telegram Premium custom-emoji reactions, and removes its
reaction set when an edited message no longer matches. Build its installable
archive with:

```bash
./modules/gaf/build-lmod.sh
```

The resulting `modules/gaf/dist/gaf.lmod` is installed through the standard
`,lm install` inspection and approval flow. Installation does not enable the
module; enable `gaf` through the local CLI and restart Lavis.
