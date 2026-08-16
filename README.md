<div align="center">

<img src="assets/logo.png" alt="Lavis logo" width="240">

# Lavis

**A personal Telegram userbot for Linux, written in Rust and packaged with Nix.**

[Pipelines](https://tangled.org/zumuvik.tngl.sh/lavis/pipelines) · [GPL-3.0-only](LICENSE) · **Alpha**

</div>

## What Lavis is

Lavis is a single-account, local Telegram userbot. It connects directly to Telegram over MTProto and accepts commands only from messages sent by the authenticated account. Command results normally edit the outgoing command message rather than creating a separate reply.

It is intentionally small: static built-in commands, local state, a reviewed external-module path, and a Nix/NixOS-first Linux workflow. It is not a bot token service, a plugin marketplace, or a remote-code-execution framework.

The command prefix is dynamic. The default is `,`, so examples use `,ping`; after changing it with `,prefix .`, use `.ping` and every other command with `.` instead.

## Install and authorize

Create a Telegram application at [my.telegram.org/apps](https://my.telegram.org/apps) and obtain an API ID and API hash. Then run the canonical Tangled flake:

```bash
nix run 'git+https://tangled.org/zumuvik.tngl.sh/lavis'
```

On first interactive use, Lavis stores API credentials locally and performs Telegram authorization: phone number, login code, and (when enabled) the two-factor password. You can run authorization explicitly with:

```bash
nix run 'git+https://tangled.org/zumuvik.tngl.sh/lavis' -- auth
```

After successful authorization, Lavis sends an invitation to Saved Messages (with a stdout fallback if sending fails). Start the in-Telegram introduction with:

```text
,start
```

If no language has been selected, choose one first:

```text
,start en
,start ru
```

The tutorial is sequential and persistent. Send `,start` for the next page, or `,start skip` to skip it. `,language [en|ru]` shows or changes the interface language without resetting aliases, modules, setup state, or tutorial state. A completed tutorial starts again from the beginning when run again.

## Commands

Replace `,` with your configured prefix.

| Command | Syntax | Purpose |
| --- | --- | --- |
| Start | `,start [en\|ru\|skip\|bot]` | Start/continue the tutorial, select its language, skip it, or enter the companion setup flow. |
| Language | `,language [en\|ru]` | Show or persist the interface language. |
| Help | `,help [command]` | List commands/modules or show a command, alias, or module card. |
| Modules | `,modules` | List built-in modules and active external commands. |
| Ping | `,ping` | Measure a live Telegram RPC latency. |
| Stats | `,stats` | Show Telegram latency, uptime, host/process information, command count, and package version. |
| Prefix | `,prefix [new-prefix\|reset]` | Show, set, or reset the command prefix. |
| Alias | `,alias [list\|add <name> <command> [arguments...]\|show <name>\|del <name>]` | Manage persistent aliases for canonical commands. |
| Fastfetch | `,fastfetch [--no-profile] [--logo <...>] [--structure <...>] [--separator <text>] [--logo-padding-left <n>] [--logo-padding-right <n>] [--logo-padding-top <n>]` | Run Fastfetch with a restricted, validated argument set. |
| Setup | `,setup [<username_bot>\|auto\|status\|repair\|cancel]` | Manage companion-bot/workspace setup in Saved Messages. |
| Modules install | `,lm [list\|info <id>\|logs <id>\|doctor [id]\|install\|confirm <approval-id>\|cancel <approval-id>\|enable <id>\|disable <id>]` | Inspect, install, and control external modules. |
| Reboot | `,reboot` | Restart the Lavis process from a fresh self-authored message. |

Examples:

```text
,help fastfetch
,prefix .
.alias add sys fastfetch --logo arch
.sys
.lm list
```

## External modules and `.lmod`

An external module is a separate executable that communicates with Lavis through a JSON-lines protocol. Modules may be written in any suitable language. Installed modules are disabled by default and do not hot-load; enable or disable changes takes effect after a Lavis restart.

To install an archive through Telegram, attach a `.lmod` file to a **new self-authored message in Saved Messages** and send:

```text
,lm install
```

Lavis inspects the attachment and shows a plan without running its code. Review it, then confirm its one-time, ten-minute approval identifier:

```text
,lm confirm XXXX-XXXX-XXXX-XXXX
,lm enable <module-id>
,reboot
```

`lm list`, `lm info`, `lm logs`, and `lm doctor` provide status and diagnostics. `lm disable <module-id>` disables a module for the next start. Declaratively managed NixOS extensions cannot be enabled or disabled from Telegram.

External modules run as the Lavis user and are **not placed in a system sandbox**. Install only code you trust. See [External modules](docs/external-modules.md), [`.lmod` packaging](docs/lmod-packaging.md), and the [module API documents](#documentation).

## Companion bot and private workspace

`<prefix>start bot` is a convenience handoff to the existing `setup` flow; it does not silently create anything. It enters the same confirmation-based BotFather conversation and uses the same persistent, idempotent setup state. There is deliberately **no** `start group` command and no standalone group-creation shortcut.

The confirmed setup creates or repairs a companion bot and a private Lavis forum workspace: a forum supergroup, General/Logs/Backups topics, the bot invitation, minimal bot rights for topic management/deleting/pinning messages, and a Lavis dialog folder. Optional official-community integration may also be attempted. Existing recorded bot/group resources are recognized and repaired rather than duplicated; interrupted setup can be inspected with `,setup status`, resumed with `,setup repair`, or locally cancelled with `,setup cancel`.

Setup is available only in Saved Messages. BotFather tokens are secret, are kept in a separate private local file, and are never shown in setup status. See [Companion bot setup](docs/companion-bot-setup.md) for the detailed resource and recovery behavior.

## Local data and security

Lavis keeps mutable data outside the Nix store under XDG paths:

```text
$XDG_CONFIG_HOME/lavis/       # credentials, companion token, Fastfetch profile
$XDG_STATE_HOME/lavis/        # MTProto session, settings, aliases, setup/module state
$XDG_DATA_HOME/lavis/         # installed modules and staging data
```

When the XDG variables are absent, these resolve below `~/.config`, `~/.local/state`, and `~/.local/share`. Settings, credentials, tokens, and state files use restrictive local permissions. Useful local CLI operations are:

```bash
lavis credentials
lavis credentials reset
lavis auth doctor
lavis auth reset --backup
lavis logout
lavis modules validate ./module.json
lavis modules enable <id>
lavis modules disable <id>
lavis modules status
```

`logout` removes the local session; it does not revoke Telegram sessions remotely. Never publish an API hash, session database, companion token, XDG directory contents, approval data, or authorization logs. Fastfetch can expose host information. BotFather messages are kept out of external-module event projection.

## NixOS service

Add the flake and import its NixOS module:

```nix
{
  inputs.lavis.url = "git+https://tangled.org/zumuvik.tngl.sh/lavis";

  outputs = { self, nixpkgs, lavis, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        lavis.nixosModules.default
        ({ ... }: {
          services.lavis = {
            enable = true;
            autoStart = false;
            credentialsEnvironmentFile = "/run/secrets/lavis.env";
          };
        })
      ];
    };
  };
}
```

The environment file may contain only literal `LAVIS_API_ID=...` and `LAVIS_API_HASH=...` values. By default the module creates a dedicated `lavis` system user with home `/var/lib/lavis`, derives XDG directories under that home, and provides `lavis-auth`.

Apply the configuration, then authorize as the service user through the supplied root-only helper before starting the service:

```bash
sudo lavis-auth
sudo systemctl start lavis.service
```

After authorization, set `services.lavis.autoStart = true` if boot startup is desired. The service uses `lavis run`, restarts on ordinary failures, and deliberately does not restart exit status `78`, which means local interactive reauthorization is required.

Available module options include `services.lavis.package`, `user`, `group`, `home`, `autoStart`, `credentialsEnvironmentFile`, `logLevel`, `settings.prefix`, `fastfetchProfile`, and declarative `extensions`. For example:

```nix
services.lavis.extensions = [
  {
    id = "gaf";
    package = inputs.lavis.packages.x86_64-linux.lavis-extension-gaf;
  }
];
```

Read [NixOS module](docs/nixos-module.md) for service recovery and declarative extension details.

## Development

```bash
git clone https://tangled.org/zumuvik.tngl.sh/lavis
cd lavis
nix develop
cargo run
```

Run the local checks:

```bash
cargo fmt --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
nix flake check
nix build
```

## Documentation

| Document | Scope |
| --- | --- |
| [Companion bot setup](docs/companion-bot-setup.md) | BotFather flow, workspace resources, recovery, and limits. |
| [NixOS module](docs/nixos-module.md) | Service, authorization, credentials, and declarative extensions. |
| [External modules](docs/external-modules.md) | Module lifecycle and operational model. |
| [`.lmod` packaging](docs/lmod-packaging.md) | Archive packaging and validation. |
| [Module API v1](docs/module-api-v1.md), [v2](docs/module-api-v2.md), [v3](docs/module-api-v3.md), [v4](docs/module-api-v4.md), [v5](docs/module-api-v5.md), [v6](docs/module-api-v6.md) | Protocol and capability evolution. |
| [Contributing](CONTRIBUTING.md) | Development contribution guidance. |

## Status and disclaimer

Lavis is alpha software. Its command behavior, module APIs, `.lmod` format, persistence schema, and Nix interfaces may change incompatibly.

Lavis is an unofficial Telegram client. Userbot use can lead to account restrictions or loss. You are responsible for your Telegram account, credentials, installed modules, compliance with Telegram rules and applicable law. The software is provided without warranty.

## License

Copyright © 2026 zumuvik. Lavis is licensed under [GNU GPL-3.0-only](LICENSE).
