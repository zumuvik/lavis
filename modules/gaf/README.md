# GAF

GAF is an external Lavis Module API v4 module for word-triggered reactions. It
is not compiled into Lavis and is distributed as `gaf.lmod`.

## Telegram commands

- `,gaf` — command menu;
- `,gaf listt` or `,gaf.listt` — list triggers;
- `,gaf setr лайк 👍 ❤️` or `,gaf.setr лайк 👍 ❤️` — set one to three reactions;
- `,gaf remt лайк` — remove a trigger;
- `,gaf toggle [on|off]` — toggle the module;
- `,gaf toggle лайк [on|off]` — toggle one trigger.

Matching is Unicode case-insensitive and uses whole-word boundaries. Reactions
from all matching triggers are de-duplicated and capped at three per message.
When an edited message no longer matches a trigger that GAF previously applied,
GAF sends an empty reaction set and removes the account reaction.

Telegram Premium custom emoji can be pasted directly after the trigger word.
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

GAF persists `state.json` beside its executable with mode `0600`.
