# CC

CC is an external Lavis Module API v6 module that rewrites `хай` to `йах` in outgoing messages.

## Commands

- `,cc.e` — enable rewriting;
- `,cc.d` — disable rewriting.

The module starts disabled and persists its enabled state. It handles new and manually edited outgoing messages. Comma-prefixed text is always ignored; commands recognized through Lavis' active command prefix are protected by the core event projection and are not exposed to modules.

Replacement is global and keeps the common casing forms: `хай` → `йах`, `Хай` → `Йах`, `ХАЙ` → `ЙАХ`.

## Build, test and install

```bash
cd modules/cc
go test ./...
go vet ./...
./build-lmod.sh
```

Install `dist/cc.lmod` through Saved Messages with `,lm install`, approve the inspection plan, enable the installed module locally, and restart Lavis.

CC requires Module API v6 support for the `message.edit` capability/action.
