# Z.AI Quota

Z.AI Quota is an external Lavis Module API v4 module that reports remaining
z.ai coding-plan quota. It is not compiled into Lavis and is distributed as
`zai.lmod`.

## Telegram commands

- `,z.ai` — quota report (module `z`, command `ai`);
- `,z` — the same report via the module default command.

The report shows every returned usage window (typically the 5-hour window and
the weekly window): used amount, total limit, percentage with a bar, remaining
amount, and the next reset time in local time.

## Token

The module reads the Bearer token from the `token` key in `~/.env`. Module
processes run with a cleared environment, so the home directory is resolved
from `$HOME` when available and from the passwd database otherwise.

## Build, test and install

```bash
cd modules/zai
go test ./...
go vet ./...
./build-lmod.sh
```

Send `dist/zai.lmod` to Saved Messages in a new message with:

```text
,lm install
```

Review the inspection plan and confirm its full approval ID within ten minutes:

```text
,lm confirm XXXX-XXXX-XXXX-XXXX
```

The installed module remains disabled. Enable it locally and restart Lavis:

```bash
lavis modules enable z
```
