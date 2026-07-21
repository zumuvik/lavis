# lavis

Minimal Rust foundation for a personal Telegram userbot. This stage authorizes one user account, persists its Telegram session locally, and handles its own outgoing `,ping` messages.

## Development

```bash
nix develop
cargo check
cargo test
nix build
nix run
nix flake check
```

Before running, provide the Telegram application credentials through the environment:

```bash
export LAVIS_API_ID='your-api-id'
export LAVIS_API_HASH='your-api-hash'
```

`nix run` prompts for a phone number and, when required, a login code and two-factor password. Login codes and passwords are entered without terminal echo. There are no automatic retries; rerun the program after an invalid credential or other authorization failure.

The default command prefix is `,`. Sending `,ping` from the authorized account edits that message to `🏓 Pong!`. Commands are handled only while Lavis is running; offline commands are not replayed on the next startup. Incoming messages, unknown commands, and other updates are ignored. Update handling is sequential. Session state defaults to `$XDG_STATE_HOME/lavis/session`, or `$HOME/.local/state/lavis/session`. The session database and any SQLite sidecar files are sensitive authentication material: keep them outside Git and do not share or copy them.
