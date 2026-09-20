# Module Packaging v1 (future)

This document specifies future packaging only. It does not authorize an
installer or modify the v6 runtime; see [ADR 0002](adr/0002-module-runtime-and-distribution.md).

## Source package

`.lmod` will be a ZIP-based source archive. It contains `module.json`,
`flake.nix`, `flake.lock`, source files, language lock files, and `README.md`.
Modules need not be Rust.

Inspection never executes package content. Extraction rejects absolute paths,
`..`, normalized traversal, symlinks, hardlinks, devices, sockets, and FIFOs.
It bounds compressed size, expanded size, file count, and individual file size.

## Nix output

A package flake exports `packages.${system}.lavisModule`, containing:

```text
share/lavis/modules/<id>/module.json
share/lavis/modules/<id>/bin/<entrypoint>
```

Runtime dependencies belong in that output or its wrapper; modules cannot rely
on inherited host `PATH`. See [Nix flakes](https://nix.dev/concepts/flakes.html)
and [`nix build`](https://nix.dev/manual/nix/stable/command-ref/new-cli/nix3-build).

## Future installation

The manager will inspect a source, lock its exact revision or digest, show
subscriptions/actions/capabilities, issue an expiring confirmation token, then
build or substitute, validate the output, probe initialize/health, and
atomically activate it.

Configured Nix substituters may be used and Nix chooses substitution versus a
local build. Lavis never automatically trusts a cache URL or public key from
module metadata; it may only display a recommended cache to the user.

Imperative installation will use a dedicated Lavis Nix profile, separate
registry metadata, and generation rollback via [`nix profile`](https://nix.dev/manual/nix/stable/command-ref/new-cli/nix3-profile).
Declarative integration is opt-in and generates a snippet or uses an explicitly
configured managed file. It never regex-edits an arbitrary `flake.nix` or runs a
NixOS/Home Manager rebuild automatically.

Path and archive validation follows the defensive parsing principles in
[Unicode TR36](https://www.unicode.org/reports/tr36/) and
[Unicode TR39](https://www.unicode.org/reports/tr39/).
