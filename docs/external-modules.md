# External Modules (alpha)

> **Status:** Alpha — manifest schemas, protocol and installer UX may change without notice. Do not treat the current API as production-stable.

External modules extend Lavis with commands implemented in any language. Each enabled module runs as a separate child process and exchanges newline-delimited JSON with Lavis through stdin/stdout.

## Поддерживаемые версии Module API

| Schema/protocol | Статус и документ |
| --- | --- |
| v1 | Внутренние метаданные, не runtime внешнего процесса: [Module API v1](module-api-v1.md). |
| v2–v3 | Базовый manifest и JSON Lines: [Module API v2/v3](module-api-v2.md). |
| v4 | Редактирование сообщений и наборы реакций: [Module API v4](module-api-v4.md). |
| v5 | Gateway статуса аккаунта: [Module API v5](module-api-v5.md). |

V1–V4 сохраняют свои существующие wire-контракты; выбор v5 не изменяет их
manifest или сообщения.

## End-to-end workflow

```text
write module
    → validate manifest
    → package stored .lmod
    → attach in Saved Messages with ,lm install
    → inspect plan
    → ,lm confirm <ApprovalId>
    → installed disabled
    → ,lm enable <id>
    → ,reboot
    → ,<id>.<command>
```

## 1. Create a module

```bash
mkdir -p my-echo/bin
```

Create `my-echo/module.json`. See [Module API v2/v3](module-api-v2.md),
[Module API v4](module-api-v4.md), and [Module API v5](module-api-v5.md) for
the schema selected by your module.

Create a directly executable entrypoint that reads JSON lines from stdin and writes JSON lines to stdout:

```bash
chmod 0755 my-echo/bin/my-echo
```

The repository includes a minimal manifest example under `examples/external-module-echo/`.

## 2. Validate locally

```bash
lavis modules validate ./my-echo/module.json
```

Validation checks identifiers, schema fields, manifest limits, directory/file type, permissions, entrypoint containment and executable mode. It does not run the module.

## 3. Package `.lmod`

A `.lmod` is a deliberately restricted ZIP archive:

- extension must end with lowercase `.lmod`;
- entries must use the ZIP **stored** method; compression and encryption are rejected;
- `module.json` must occur exactly once at archive root;
- do not wrap the files in an extra top-level directory;
- only regular files and directories are accepted;
- symlinks, devices, FIFOs and special permission bits are rejected;
- executable mode of the entrypoint must be preserved.

Example with Info-ZIP:

```bash
cd my-echo
zip -0 -X -r ../my-echo.lmod module.json bin
cd ..
```

Check the archive before sending:

```bash
unzip -l my-echo.lmod
zipinfo -l my-echo.lmod
```

See [Packaging `.lmod`](lmod-packaging.md) for exact limits and failure cases.

## 4. Inspect and install in Telegram

Use a **new self-authored message in Saved Messages**. Attach exactly one `.lmod` document to the same message and use the current prefix:

```text
,lm install
```

Edited messages do not start or confirm installation. URL, repository and reply-based acquisition are not supported.

Lavis performs a bounded download and fail-closed archive inspection. The returned plan includes module ID/version, protocol version, entrypoint, capabilities, archive statistics, SHA-256 digest, fingerprint, warnings and expiry.

The plan contains a random 80-bit ApprovalId in canonical form:

```text
XXXX-XXXX-XXXX-XXXX
```

It expires exactly 10 minutes after issue.

Confirm:

```text
,lm confirm K7F4-M2PX-9QDT-7R6N
```

Cancel:

```text
,lm cancel K7F4-M2PX-9QDT-7R6N
```

The full ID is required; prefix matching and reuse are not supported.

## 5. Understand installation semantics

On confirmation Lavis:

1. redeems the approval once;
2. atomically renames the validated payload into `$XDG_DATA_HOME/lavis/modules/<id>/` with no-replace semantics;
3. refuses cross-filesystem copy fallback;
4. validates `module.json` again from the final target path;
5. rolls the target back if final validation fails;
6. registers the descriptor in the running manager without starting a process or changing enabled state.

An existing target is never overwritten. Installation guarantees atomic visibility during normal operation; it does not claim full power-loss durability.

## 6. List installed modules

Telegram:

```text
,lm
,lm list
```

The list includes discovered descriptors even when they are disabled. Runtime statuses include:

- `установлен, выключен`
- `активен`
- `ошибка`
- `остановлен`

Local CLI:

```bash
lavis modules status
```

## 7. Inspect, enable and run

Installation intentionally leaves the module disabled.

Read the installed-module list or its metadata in Telegram:

```text
,lm list
,lm info my-echo
```

Enable a manually managed module in a new self-authored Saved Messages message:

```text
,lm enable my-echo
,reboot
```

Enablement is persistent. It changes the next application start only: enabled modules start
during application startup, not on first command. There is no live enable/disable or hot
reload. `,reboot` restarts the Lavis application process; it does not reboot the host. It edits
the same command message first to `♻️ Lavis перезапускается…` and, after successful startup, to
`✅ Lavis перезагрузился` with whole elapsed seconds, truncated from milliseconds; it creates no separate message.

For security, `,lm enable`, `,lm disable` and `,reboot` are accepted only from a new,
self-authored Saved Messages message. Edited messages do not qualify.

The existing local CLI flow remains available:

```bash
lavis modules enable my-echo
```

Restart Lavis after using the CLI, or send `,reboot` from a qualifying Saved Messages message.

Namespaced invocation:

```text
,my-echo.echo Hello from Lavis!
```

A schema 3 manifest may declare `default_command`, which additionally enables:

```text
,my-echo Hello from Lavis!
```

Disable without deleting files (also from a new self-authored Saved Messages message):

```text
,lm disable my-echo
```

The running process is stopped during normal Lavis shutdown. The local equivalent is
`lavis modules disable my-echo`.

### Scenario: install and activate a module

1. Attach `my-echo.lmod` to a new self-authored Saved Messages message and send `,lm install`.
2. Inspect the plan, then send `,lm confirm <ApprovalId>`.
3. Send `,lm enable my-echo` in a new self-authored Saved Messages message.
4. Send `,reboot` in another new self-authored Saved Messages message.
5. After Lavis restarts, invoke `,my-echo.echo Hello`.

## CLI reference

These are local terminal commands, not Telegram commands.

### `lavis modules validate <path>`

Validate a module manifest without installing, enabling or running the module.

### `lavis modules enable <id>`

Add an already installed/discovered module ID to persistent enabled state. The module starts on the next Lavis launch.

### `lavis modules disable <id>`

Remove an ID from persistent enabled state. Does not delete module files.

### `lavis modules status`

Show discovered modules and persistent enabled state.

## Telegram `lm` reference

```text
,lm
,lm list
,lm info <id>
,lm install
,lm confirm <ApprovalId>
,lm cancel <ApprovalId>
,lm enable <id>
,lm disable <id>
,help lm
```

`install`, `confirm`, `cancel`, `enable` and `disable` are state-changing and require a new
own message in Saved Messages. `lm`, `lm list` and `lm info <id>` are read-only.

## Command and help resolution

Resolution order:

1. built-in canonical commands;
2. active external namespaced commands;
3. active schema 3 default commands;
4. aliases.

Help for a discovered module card is available by module ID even while it is disabled. Command-specific help requires the command to be active:

```text
,help my-echo
,help my-echo.echo
```

`,lm list` remains the authoritative user-facing status list for all installed descriptors.

## Writing modules

### Required process behavior

- validate `protocol_version` on every request;
- respond to `initialize`, `execute`, `health` and `shutdown`;
- echo the exact decimal `request_id`;
- emit one complete JSON object per line;
- flush stdout after each reply;
- start quickly;
- use stderr only for diagnostics.

Schema 3 may also receive `message.created` events and return one scoped
`message.react` action. Schema 4 adds edited-message support; schema 5 adds the
allowlisted core account-status gateway. See [Module API v5](module-api-v5.md);
v2–v4 behavior remains unchanged.

### Environment

Lavis clears the inherited environment and supplies:

- `NO_COLOR=1`
- `CLICOLOR=0`
- `CLICOLOR_FORCE=0`
- `TERM=dumb`
- a fixed minimal `PATH`

Telegram credentials and arbitrary host environment variables are not passed to the module. Entrypoints are executed directly; Lavis does not invoke a shell.

### Capabilities

Capabilities do not form an OS sandbox. A module retains the ordinary OS access
of the Lavis user. In schema 5, `telegram.account.status` is enforced by the
core gateway boundary; it controls that core-mediated feature, not direct OS
access. See [Module API v5](module-api-v5.md).

## Files and lifecycle

Installed modules:

```text
$XDG_DATA_HOME/lavis/modules/<id>/
```

Persistent enabled state:

```text
$XDG_STATE_HOME/lavis/external-modules.json
```

Private installer staging:

```text
$XDG_DATA_HOME/lavis/module-staging/
```

If XDG variables are unset, Lavis uses the corresponding `$HOME/.local/share` and `$HOME/.local/state` locations.

Pending staging is cleaned best-effort on cancel, expiry, shutdown and next startup. Only wrappers with Lavis' strict ownership marker are eligible for abandoned cleanup.

## Security

- `.lmod` inspection is structural validation, not malware analysis or signature verification.
- External modules run as arbitrary executable code with the Lavis user's OS permissions.
- No seccomp, container, WASM runtime or other system sandbox is applied.
- Schema-5 `telegram.account.status` is enforced only at the core gateway
  boundary; it is not an OS permission.
- The installer never overwrites an existing module target.
- Telegram acquisition is limited to a same-message document in Saved Messages and bounded by declared and actual bytes.
- Enable only code you trust.

## Current limitations

- no live enable/disable or hot reload;
- `reboot` restarts Lavis only; it is not an operating-system reboot;
- no update, replace or uninstall command;
- no remote repository or URL acquisition;
- no registry, signatures or trust store;
- no dependency build/install system;
- no module-to-module communication;
- no system sandbox;
- no media-group assembly or reply-to-document install;
- no power-loss durability protocol beyond normal atomic visibility.

## Troubleshooting

### `.lmod` is rejected

Check:

- filename uses lowercase `.lmod`;
- archive uses stored entries (`zip -0`), not deflate;
- `module.json` is at root, not under `my-module/`;
- entrypoint is included and executable;
- archive has no symlinks or special files;
- limits in [Packaging `.lmod`](lmod-packaging.md) are respected.

### Manifest validation fails

```bash
lavis modules validate ./my-module/module.json
```

### Installed module is not callable

Installation does not enable it. Run:

```bash
lavis modules enable <id>
```

Then restart Lavis and check `lavis modules status` plus `,lm list`.

### Module does not respond

Confirm the entrypoint has a valid shebang or is a native executable, is executable, follows the selected JSON protocol and flushes stdout. Enable safe diagnostics with:

```bash
RUST_LOG=lavis=debug lavis
```

Module stderr is drained continuously while the module runs and the first 16 KiB are kept. When a module crashes, fails the protocol, times out or fails handshake, the captured stderr is included in the structured `external_module_crashed` event at error level; invalid UTF-8 is converted lossy. A normal shutdown does not create a crash event, and a protocol-valid application `error` reply during execute is not a crash either: it does not emit `external_module_crashed`, does not terminate the module process, and the module stays ready for subsequent requests. Never write secrets to stderr.
