# Typing

Standalone Lavis API v6 module. Build with `./build-lmod.sh`, then install `dist/typing.lmod` using Lavis. `,typing on [@username|ID]` adds an active target (up to 6); `,typing list` shows all active targets and elapsed time (HH:MM:SS). `,typing off @username` (or ID) stops one target; `,typing off` stops them all. `,typing target @username` selects the default for `,typing on`. Without a selected default, `on` uses the current chat. Numeric dialog IDs work when the peer is accessible in your dialogs. Active indicators are in memory and stop when the module exits.

**Security:** Installation requests `telegram.raw` and `raw.invoke`, which grant unrestricted Telegram RPC access to this module. Review its source before approving. Numeric IDs alone cannot resolve Telegram access hashes for unknown peers; specify a username or open the conversation first.
