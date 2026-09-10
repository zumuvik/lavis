# Module runtime roadmap

This roadmap tracks remaining work for the external-module runtime. Completed
milestones — the API v6 foundation, v6 observability, Telegram authorization
diagnostics and session recovery, and the v6 alpha conformance kit — were
removed after they shipped; Git history, `docs/module-api-v6.md`, and
`docs/external-modules.md` remain the implementation record.

## Current baseline

Lavis currently has:

- external module protocols v2-v6 with manifest validation and capability
  declarations;
- `.lmod` inspection, staging, approval, atomic installation, and control UX;
- persistent module state and declarative NixOS integration, including a
  declarative/imperative enabled-state merge that protects Nix-managed modules
  from imperative mutation;
- a persistent API v6 module supervisor with strict lifecycle framing,
  parentless `telegram.invoke` calls, curated typed Telegram helpers, explicit
  `telegram.raw`, and the stable `raw.invoke` TL escape hatch;
- an offline conformance runner (`lavis-v6-conformance`) driven by the frozen
  alpha wire-contract fixture and offline host transcripts;
- per-module RPC concurrency limits in addition to the global v6 RPC
  semaphore, and flood-wait retry metadata on typed call errors;
- maintained example v6 modules packaged as reproducible `.lmod` archives
  (gaf, zai, cleaner);
- sanitized authorization failure categories, an exclusive local session lock,
  terminal-auth manual recovery commands (`lavis auth doctor`,
  `lavis auth reset --backup`), and no restart-loop on invalidated sessions;
- companion group identity delivered to `contract_revision: 4` modules inside
  the execute context, so modules never need title-based chat discovery;
- bounded, single-line module error messages surfaced in command replies and
  structured logs instead of a generic `Unavailable` wall;
- user-facing `lm info`, `lm logs`, and `lm doctor` diagnostics, plus CLI
  `lavis modules validate|status|enable|disable`.

The completeness rule for API v6 is:

> A module must be able to use a Telegram RPC that Lavis has never heard of
> without changing or rebuilding Lavis.

Typed helpers are convenience and policy surfaces. They are not the ceiling of
what a v6 module can do. A module that explicitly receives `telegram.raw` and
the `raw.invoke` grant owns TL serialization/deserialization for raw calls.

## Priority 1: installation, update, and rollback lifecycle

Complete the imperative lifecycle without conflating runtime registration with
filesystem installation. This is the largest remaining gap: installing works,
but replacing or removing a module currently requires filesystem access
outside Telegram.

- Fix approvals that remain pending after an early duplicate-module rejection.
- Detect duplicate installed IDs before issuing an approval where possible.
- Add atomic update with version and digest comparison:
  - `lm update [<id>] <.lmod>` reuses the approval flow to replace an existing
    module ID while keeping the previous generation until the new one passes
    manifest validation and an optional startup health check.
- Add remove and rollback operations with state-preservation rules:
  - `lm uninstall <id>` stops the module, removes its catalog directory, and
    applies the documented runtime-state preservation rule;
  - rollback restores the previous generation after a failed update.
- Keep catalog, enabled state, running handles, and `lm doctor`/`lm list`
  reporting consistent across install/update/uninstall, including the case
  where a catalog directory is removed while a module process is still in
  memory. The `lm doctor` missing-catalog condition bug was fixed in `ca00c8f`;
  the lifecycle events that drive the same reconciliation still need to be
  implemented.
- Record package source, digest, granted capabilities, granted methods, install
  time, and active generation.
- Distinguish installed, registered, enabled, running, and crashed states in
  UX, and keep `,modules`/`,lm list` terminology explicit so disabled modules
  do not appear to disappear.

## Priority 2: module developer experience

Add local tooling that uses the same validation and process runtime as the
production application:

- `lavis modules dev <path>` development runner:
  - explicit debug logging without globally enabling noisy dependency traces;
  - pretty-printed protocol frames with sensitive fields redacted;
  - captured stdout/stderr and exit status;
  - deterministic fixture Telegram responses and raw-TL request/response
    fixtures;
  - timeout and malformed-response simulation;
  - manifest and capability diagnostics with file/field context;
- `lavis modules doctor <id>` and `lavis modules logs <id>` CLI parity with
  the in-chat `lm doctor`/`lm logs`;
- generation of a reproducible `.lmod` package from a module directory;
- reference SDK helpers for Go and Rust that make raw calls ergonomic without
  making Lavis understand the method: for Rust a helper that serializes any
  `RemoteCall`; for other languages the language's Telegram TL schema
  implementation;
- a migration document covering the v5 host-call surface to the v6 persistent
  RPC model.

## Priority 3: compatibility and deprecation

- Keep v2-v5 operational while v6 is alpha.
- Backport observability improvements to legacy processes.
- Do not require existing modules to migrate merely to receive crash
  diagnostics.
- Persist last successful upstream `info` metadata across restarts so cold
  starts can render stale-good data while background refresh runs; track this
  separately in #42 and only reuse revision relations for the same local build
  SHA.
- Publish a deprecation policy only after v6 has a stable specification,
  conformance suite, reference SDKs, and at least two migrated production
  modules.
- Treat protocol removal as a major compatibility event with an explicit
  migration window.

## Priority 4: curated Telegram helper expansion (optional)

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
