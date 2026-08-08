# GAF

GAF is an external Lavis Module API v4 module for word-triggered reactions. It
is not compiled into Lavis and is distributed as `gaf.lmod`.

## Telegram commands

- `,gaf` — command menu;
- `,gaf listt` or `,gaf.listt` — list triggers;
- `,gaf setr никс | 👍 ❤️` or `,gaf.setr никс | 👍 ❤️` — set one to three reactions;
- `,gaf remt лайк` — remove a trigger;
- `,gaf toggle [on|off]` — toggle the module;
- `,gaf toggle лайк [on|off]` — toggle one trigger.

Matching is Unicode case-insensitive. A trigger must start at a word boundary,
but it can match the beginning of a longer word: `фур` matches `фур`, `фури`
and `фурре`, while it does not match `антифур`. Reactions from all matching
triggers are de-duplicated and capped at three per message. When an edited
message no longer matches a trigger that GAF previously applied, GAF sends an
empty reaction set and removes the account reaction.

Rules use the explicit `trigger | reactions` format. Telegram Premium custom emoji can be pasted directly on the right side of `|`, for example `,gaf setr никс | <custom-emoji>`.
For diagnostics, `ce:<document_id>` and `custom:<document_id>` are also accepted.

## Build, test and install

```bash
cd modules/gaf
go test ./...
go vet ./...
./build-lmod.sh
```

Send `dist/gaf.lmod` to Saved Messages in a new message with:

```text
,lm install
```

Review the inspection plan and confirm its full approval ID within ten minutes:

```text
,lm confirm XXXX-XXXX-XXXX-XXXX
```

The installed module remains disabled. Enable it locally and restart Lavis:

```bash
lavis modules enable gaf
```

GAF persists `state.json` in `LAVIS_MODULE_STATE_DIR` when Lavis provides it,
or `$XDG_STATE_HOME/lavis/modules/gaf/` when run directly.
