# Module runtime roadmap

This roadmap defines the order in which the external-module runtime should be
stabilized and extended. Runtime observability and recovery are release gates,
not optional follow-up work.

## Current baseline

Lavis currently has:

- external module protocols v2-v5;
- manifest validation and capability declarations;
- `.lmod` inspection, staging, approval, atomic installation, and control UX;
- persistent module state and declarative NixOS integration;
- bounded process I/O, request correlation, lifecycle events, scheduler calls,
  companion-bot calls, and Telegram gateway actions;
- backwards-compatible discovery and command routing for installed modules.

Draft PR #28 introduces the API v6 foundation:

- a persistent module process supervisor;
- strict v6 lifecycle and `telegram.invoke` frames;
- bounded queues, pending requests, RPC workers, and shutdown handling;
- an explicit `telegram.raw` capability with reviewed per-module method grants;
- a generated typed method registry;
- a dedicated Telegram client with retries and peer caching disabled;
- a global RPC concurrency limit;
- the first reviewed methods: `account.updateStatus`, `contacts.getContacts`,
  `messages.getHistory`, and `messages.getDialogs`.

PR #28 is still a draft. Its current CI run stops at `cargo fmt --check`, so
compilation, Clippy, tests, `nix flake check`, and package builds have not yet
run for the current head.

## Priority 0: module observability and failure recovery

This work must land before expanding the v6 method surface.

### Structured failure records

Every module termination must retain a bounded diagnostic record containing:

- module ID and protocol version;
- lifecycle stage: spawn, initialize, execute, event, health, RPC, or shutdown;
- request ID or event kind when available;
- stable error category;
- process exit status or signal;
- bounded and UTF-8-lossy stderr;
- whether output was truncated;
- timestamp and restart generation.

Diagnostics must never include Telegram credentials, session data, raw access
hashes, environment secrets, or unrestricted request payloads.

### Runtime logging

- Log failed lifecycle dispatches instead of discarding their results.
- Forward valid module `log` frames into `tracing` with module metadata.
- Log protocol decode failures with the frame type and validation category,
  without dumping arbitrary untrusted payloads.
- Preserve stderr after process exit instead of aborting and discarding the
  capture task.
- Distinguish clean shutdown, module-reported error, protocol violation,
  timeout, transport failure, and unexpected child exit.

### User-facing diagnostics

Add commands equivalent to:

```text
,lm info <id>
,lm logs <id>
,lm doctor <id>
```

`lm info` must distinguish these states:

- discovered;
- installed and disabled;
- enabled but not started;
- running;
- crashed;
- invalid manifest;
- missing module directory;
- declaratively managed.

A module developer must be able to identify a malformed `event_result`, a
missing required field, a bad request ID, or stderr failure without using
`strace` or reading Lavis source code.

### Regression coverage

Add process fixtures for:

- malformed JSON;
- missing required `actions` in `event_result`;
- wrong request ID;
- wrong module ID during initialization;
- oversized line and result;
- immediate process exit;
- non-zero exit after successful initialization;
- stderr before crash;
- lifecycle event failure;
- timeout and forced termination.

### Acceptance gate

Given a fixture that returns an invalid lifecycle response, Lavis must:

1. mark the module as crashed;
2. terminate and reap the complete process group;
3. retain the bounded diagnostic record;
4. emit a structured warning containing the exact failure category;
5. expose the same category through `lm info` or `lm logs`;
6. keep other modules and the Telegram update loop operational.

## Priority 1: finish the API v6 foundation in PR #28

### CI and source quality

- Run `cargo fmt` and make formatting CI green.
- Pass compilation, Clippy, unit tests, `nix flake check`, and package builds.
- Add a CI check that regenerates `src/external_modules/v6_registry.rs` from
  `tools/v6-methods.json` and fails on a dirty diff.
- Keep protocols v2-v5 byte-compatible and covered by regression tests.

### Supervisor correctness

- Add end-to-end tests using a real v6 fixture process, not only helper-unit
  tests.
- Verify initialize, execute, event, health, concurrent RPC, timeout, shutdown,
  EOF, and forced-kill paths.
- Record the first terminal failure instead of collapsing all later requests
  into `Unavailable`.
- Make child exit status part of shutdown success criteria.
- Provide a real force-termination operation; `terminate()` must not merely
  retry graceful shutdown.
- Define queue-full behavior explicitly. Temporary backpressure must not be
  indistinguishable from a protocol crash.
- Verify that dropped handles cannot leak a child, writer, reader, RPC worker,
  or process group.
- Retain and publish stderr and module log frames through the Priority 0
  diagnostic path.

### Protocol contract

Create `docs/module-api-v6.md` covering:

- lifecycle frame schemas;
- parentless `telegram.invoke` and `telegram.result` correlation;
- request-ID and call-ID requirements;
- JSON depth, string, collection, line, and result limits;
- queue, concurrency, and timeout semantics;
- shutdown behavior;
- error categories and retry expectations;
- capability and per-method grant rules;
- compatibility guarantees for v2-v5.

### Security gate

API v6 must remain a reviewed allowlist, not a generic MTProto dispatcher.
Before merge, verify:

- no method can be invoked without `telegram.raw`;
- every method must also appear in the module's `telegram_methods` grant;
- grants are included in the installation fingerprint and confirmation plan;
- parameters use strict typed decoding with unknown fields rejected;
- outputs are bounded and intentionally shaped;
- access hashes, sessions, credentials, and unrestricted peer objects are not
  exposed;
- RPC errors returned to modules are sanitized;
- global and per-module resource limits prevent one module from starving the
  runtime.

### Acceptance gate

PR #28 can leave draft status only when all CI stages pass and a packaged v6
fixture completes initialize, execute, event dispatch, at least one approved
Telegram RPC, health, and graceful shutdown under an integration test.

## Priority 2: API v6 alpha and module conformance kit

After PR #28 is stable:

- freeze an alpha wire contract and publish examples;
- provide a protocol conformance runner for third-party modules;
- add reference SDK helpers for Go and Rust;
- provide deterministic fixtures for success, RPC error, timeout, cancellation,
  malformed frames, duplicate call IDs, and shutdown races;
- add a maintained example v6 module packaged as `.lmod`;
- document migration from v5 host calls to v6 persistent RPC;
- add per-module concurrency and fairness limits in addition to the global
  semaphore;
- expose retry metadata only where it is safe and actionable.

No additional Telegram method should be added until the conformance kit can
exercise its parameter validation, output shape, failure mapping, and resource
limits.

## Priority 3: reviewed Telegram method expansion

Expand `tools/v6-methods.json` in small reviewable batches.

Each new method requires:

1. a documented use case;
2. a strict input type;
3. explicit peer restrictions;
4. a bounded output type;
5. capability and privacy review;
6. unit and integration tests;
7. installation-plan visibility;
8. a compatibility note.

Prefer purpose-built, minimal result objects over serialized raw Telegram TL
objects. Methods exposing message bodies, arbitrary peers, media, membership,
contacts, or account mutations require separate threat-model review.

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
- timeout and malformed-response simulation;
- manifest and capability diagnostics with file/field context;
- generation of a reproducible `.lmod` package.

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

## Global acceptance gates

Every runtime or protocol PR must satisfy all applicable gates:

- formatting, compilation, Clippy, tests, flake check, and package build pass;
- no unbounded queue, collection, output, stderr capture, or task growth;
- no child process or process-group leak on any exit path;
- no secrets in module-visible errors or diagnostics;
- existing protocol fixtures remain green;
- new wire behavior is documented before merge;
- user-facing state matches actual filesystem and process state;
- a failure produces an actionable diagnostic rather than only `Unavailable`.

## Explicit non-goals

- A generic unrestricted Telegram/MTProto proxy for modules.
- Treating process isolation as a security sandbox.
- Loading unreviewed native code in the Lavis process.
- Building arbitrary source code received through Telegram.
- Silently modifying user Nix configuration.
- Deprecating working legacy modules before v6 tooling and migration paths are
  complete.
