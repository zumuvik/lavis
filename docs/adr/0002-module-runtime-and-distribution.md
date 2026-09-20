# ADR 0002: Separate module runtime from distribution

## Status

Accepted.

## Decision

Lavis separates three concerns:

1. **Module API v3** defines the child-process runtime protocol, subscriptions,
   command context, and typed actions.
2. **Module Packaging v1** defines future source packages and their reproducible
   Nix build output.
3. A future **module manager** will inspect, confirm, install, update, remove,
   and roll back packages through a built-in `,lm` UX.

The normative runtime contract is [Module API v6](../module-api-v6.md). The
future package contract is [Module Packaging v1](../module-packaging-v1.md).
The staged delivery plan is [Module roadmap](../module-roadmap.md).

## Rationale

Replying `,lm` to arbitrary source code must not compile or run it. Installation
needs inspection, an explicit expiring confirmation, locked inputs, and output
validation. Telegram credentials, session material, the grammers client, raw
peer/message identifiers, and every MTProto call remain exclusively in Lavis
core.

Nix owns reproducible builds and substitution. Lavis may ask Nix to build a
previously approved package, but must not invent cache trust or execute package
metadata. The installer is deferred until the runtime contract is stable.

## Planned and unsupported sources

Initial future package sources are a `.lmod` source archive, a pinned remote Nix
flake revision, and an already built Nix output matching the module layout.

The initial manager will not accept bare `.rs`, a bare Python project with
uncontrolled pip dependencies, arbitrary install shell scripts, unpinned
repositories, or automatic trust of third-party cache keys.

## Consequences

External modules remain unsandboxed child processes. Runtime capabilities are
core-enforced allow-lists, not an operating-system sandbox. Packaging and
installation are intentionally not implemented by Module API v3.
