# Typing

Standalone Lavis API v6 module. Build with `./build-lmod.sh`, then install `dist/typing.lmod` using Lavis. Enable periodic typing with `,typing on`, stop with `,typing off`, or select a destination with `,typing target @username` (numeric dialog IDs also work when the peer is accessible in your dialogs). An optional destination may follow `on`; without one, the previously selected target or current chat is used. The active indicator is in memory and stops when the module exits.

**Security:** Installation requests `telegram.raw` and `raw.invoke`, which grant unrestricted Telegram RPC access to this module. Review its source before approving. Numeric IDs alone cannot resolve Telegram access hashes for unknown peers; specify a username or open the conversation first.
