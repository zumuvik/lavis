# Module runtime roadmap

This roadmap tracks remaining work for the external-module runtime. Completed
API v6 foundation and v6 observability milestones were removed after PR #28
merged; Git history and `docs/module-api-v6.md` remain the implementation record.

## Current baseline

Lavis currently has:

- external module protocols v2-v6;
- manifest validation and capability declarations;
- `.lmod` inspection, staging, approval, atomic installation, and control UX;
- persistent module state and declarative NixOS integration;
- backwards-compatible discovery and command routing for installed modules;
- a persistent API v6 module supervisor with strict lifecycle framing;
- parentless `telegram.invoke` calls and curated typed Telegram helpers;
- explicit `telegram.raw` plus the stable `raw.invoke` TL escape hatch;
- generated v6 method metadata shared by manifest and runtime validation;
- bounded lifecycle/RPC queues, concurrency, timeouts, shutdown handling, and
  complete process-group termination;
- retained v6 crash diagnostics, stderr/module-log capture, and user-facing
  `lm info`, `lm logs`, and `lm doctor` diagnostics;
- a dedicated Telegram client/transport and a global v6 RPC concurrency limit.

The completeness rule for API v6 is:

> A module must be able to use a Telegram RPC that Lavis has never heard of
> without changing or rebuilding Lavis.

Typed helpers are convenience and policy surfaces. They are not the ceiling of
what a v6 module can do. A module that explicitly receives `telegram.raw` and
the `raw.invoke` grant owns TL serialization/deserialization for raw calls.

## Priority 0B: Telegram authorization diagnostics and session recovery

This is an independent authorization/session-recovery milestone.

Authorization failures must preserve an actionable, sanitized cause instead of
collapsing every `Client::is_authorized()` failure into
`failed to check Telegram authorization status`.

The observed `AUTH_KEY_DUPLICATED` failure is the reference incident for this
work: the dependency log contained the exact Telegram RPC error, while Lavis
returned only the generic `AuthError::AuthorizationCheck` wrapper.

### Error classification

- Preserve the safe Telegram RPC code and symbolic error name when available.
- Distinguish RPC rejection, transport failure, timeout, session-storage
  failure, malformed local session state, and interactive-authentication
  failure.
- Keep a stable internal category such as `auth_key_duplicated` while retaining
  the original sanitized Telegram name for diagnostics.
- Do not require global dependency debug logging to discover the root cause.
- Keep the full error chain available to `anyhow` and structured `tracing`
  without exposing secrets.

For `AUTH_KEY_DUPLICATED`, startup output must explain that the stored auth key
has been invalidated and that retrying the same session is not sufficient.

For the NixOS service, a terminal Lavis application exit must not enter a
systemd restart loop. The module must preserve restarts for abnormal process
termination while preventing restart of Lavis' terminal status `1`.

### Single-session protection

- Add an exclusive local lock associated with the session path before opening
  the Telegram client.
- Report the PID or service context holding the lock where this can be done
  safely.
- Detect and explain the common conflict between an interactive `lavis` process
  and `lavis.service` using the same session.
- Document that a copied session must not be used concurrently on another host
  or through independently routed connections.
- Release the lock on every clean, failed, cancelled, and panic-unwind exit path
  supported by the runtime.

A local lock cannot prevent another host from reusing a copied session, but it
must prevent the most common same-machine duplication.

### Recovery UX

Add commands equivalent to:

```text
lavis auth doctor
lavis auth reset --backup
```

`auth doctor` should report, without reading out secret contents:

- resolved session path;
- whether a local process lock is held;
- session file type, ownership, permissions, and basic readability;
- last sanitized authorization failure category;
- whether reauthorization is required;
- whether the application appears to be running as both a service and an
  interactive process.

`auth reset --backup` must:

1. refuse to run while another process owns the session lock;
2. stop before modifying an active session;
3. atomically move the session database and sidecar files into a timestamped
   backup directory;
4. preserve restrictive permissions;
5. print the backup path;
6. require a fresh Telegram login on the next start.

Lavis must never print or persist the phone number, login code, 2FA password,
API hash, auth key, session bytes, or unrestricted Telegram response payloads
as part of diagnostics.

### Authorization regression coverage

Add injectable or fixture-backed tests for:

- `AUTH_KEY_DUPLICATED` returned by `is_authorized()`;
- generic RPC rejection with a sanitized symbolic name;
- transport disconnect and timeout;
- corrupt or unreadable session storage;
- a second local process attempting to acquire the session lock;
- backup/reset with SQLite-style `-wal` and `-shm` sidecar files;
- successful reauthorization after reset;
- confirmation that secrets never appear in error text, logs, or diagnostics.

### Acceptance gate

Given an `AUTH_KEY_DUPLICATED` fixture, Lavis must:

1. retain the `auth_key_duplicated` category;
2. emit a structured error with the sanitized RPC name;
3. explain that the current session cannot be reused;
4. provide the exact supported recovery command;
5. exit without repeatedly retrying the invalidated key;
6. expose no credential or session secret in normal or debug output.

## Priority 2: API v6 alpha and module conformance kit

With the v6 foundation merged:

- freeze an alpha wire contract and publish examples;
- provide a protocol conformance runner for third-party modules;
- add reference SDK helpers for Go and Rust;
- provide deterministic fixtures for success, RPC error, timeout, cancellation,
  malformed frames, duplicate call IDs, raw TL calls, and shutdown races;
- add a maintained example v6 module packaged as `.lmod`;
- document migration from v5 host calls to v6 persistent RPC;
- add per-module concurrency and fairness limits in addition to the global
  semaphore;
- expose retry metadata only where it is safe and actionable.

The conformance kit must test both curated helpers and `raw.invoke`; module
authors must not need a Lavis source checkout to determine whether their module
speaks v6 correctly.

## Priority 3: curated Telegram helper expansion

`raw.invoke` is the completeness mechanism. Expanding `tools/v6-methods.json`
is optional developer-experience work, not a prerequisite for new modules.

Add a purpose-built helper only when it provides meaningful value such as:

1. simpler parameters than raw TL;
2. stable high-level peer handling;
3. redacted/minimal result objects;
4. a lower-risk capability than arbitrary raw access;
5. common retry or pagination behavior;
6. a clearly testable compatibility contract.

A new Telegram RPC must never require a new Lavis release merely because no
typed helper exists for it. Modules with explicit raw authority can use their
own TL library/schema and `raw.invoke` immediately.

## Priority 4: module developer experience

Add local tooling that uses the same validation and process runtime as the
production application:

```text
lavis modules validate <path>
lavis modules dev <path>
lavis modules doctor <id>
lavis modules logs <id>
lavis modules status
```

The development runner should support:

- explicit debug logging without globally enabling noisy dependency traces;
- pretty-printed protocol frames with sensitive fields redacted;
- captured stdout/stderr and exit status;
- deterministic fixture Telegram responses;
- deterministic raw-TL request/response fixtures;
- timeout and malformed-response simulation;
- manifest and capability diagnostics with file/field context;
- generation of a reproducible `.lmod` package.

Reference SDKs should make raw calls ergonomic without making Lavis understand
the method. For Rust this can be a helper that serializes any `RemoteCall`; for
other languages it can use the language's Telegram TL schema implementation.

## Priority 5: installation, update, and rollback lifecycle

Complete the imperative lifecycle without conflating runtime registration with
filesystem installation.

- Fix approvals that remain pending after an early duplicate-module rejection.
- Detect duplicate installed IDs before issuing an approval where possible.
- Distinguish installed, registered, enabled, running, and crashed states in UX.
- Add atomic update with version and digest comparison.
- Add remove and rollback operations with state-preservation rules.
- Preserve the previous generation until the replacement has passed manifest
  validation and an optional startup health check.
- Record package source, digest, granted capabilities, granted methods, install
  time, and active generation.
- Make `,modules` and `,lm list` terminology explicit so disabled modules do not
  appear to disappear.

## Priority 6: declarative Nix integration

Declarative modules must remain reproducible and must not be silently mutated
by imperative commands.

- Pin packages and module IDs through the NixOS module.
- Merge declarative and imperative enabled state without deleting unrelated
  manual modules.
- Keep runtime state outside immutable package directories.
- Validate package contents before activation.
- Support binary caches without executing arbitrary build instructions from a
  Telegram attachment.
- Never rewrite arbitrary user Nix configuration.
- Document ownership, migration, garbage collection, and rollback behavior.

## Priority 7: compatibility and deprecation

- Keep v2-v5 operational while v6 is alpha.
- Backport observability improvements to legacy processes.
- Do not require existing modules to migrate merely to receive crash
  diagnostics.
- Publish a deprecation policy only after v6 has a stable specification,
  conformance suite, reference SDKs, and at least two migrated production
  modules.
- Treat protocol removal as a major compatibility event with an explicit
  migration window.
- Persist last successful upstream `info` metadata across restarts so cold
  starts can render stale-good data while background refresh runs; track this
  separately in #42 and only reuse revision relations for the same local build
  SHA.

## Global acceptance gates

Every runtime or protocol PR must satisfy all applicable gates:

- formatting, compilation, Clippy, tests, flake check, and package build pass;
- no unbounded queue, collection, output, stderr capture, or task growth;
- no child process or process-group leak on any exit path;
- no intentional exposure of secrets or raw TL bodies in module-visible
  diagnostics or logs (module-controlled stderr remains untrusted);
- existing protocol fixtures remain green;
- new wire behavior is documented before merge;
- user-facing state matches actual filesystem and process state;
- module failures produce actionable diagnostics rather than only
  `Unavailable`;
- changes affecting Telegram authorization or session management preserve
  actionable sanitized authorization categories rather than collapsing
  failures to `AuthorizationCheck`.

## Explicit non-goals

- Implicit raw Telegram authority without an explicit install-time capability
  and grant.
- Transmitting Telegram credentials, auth keys, session bytes, or sender handles
  through Module API v6 IPC.
- Treating process isolation or capabilities as an OS security sandbox; the v6
  threat model is an API/IPC guarantee, not sandboxing.
- Loading unreviewed native code in the Lavis process.
- Building arbitrary source code received through Telegram.
- Silently modifying user Nix configuration.
- Printing or persisting Telegram credentials, auth keys, session contents, or
  raw TL bodies for debugging.
- Deprecating working legacy modules before v6 tooling and migration paths are
  complete.
