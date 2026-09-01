<div align="center">

<img src="https://tangled.org/zumuvik.tngl.sh/lavis/raw/main/assets/logo.png" alt="Lavis logo" width="240">

# Lavis

**Fast and extensible Telegram userbot written in Rust**

Runs directly over MTProto, targets Linux, and provides first-class declarative NixOS integration.

<p>
  <a href="https://tangled.org/zumuvik.tngl.sh/lavis/blob/main/README.ru.md">
    <img src="https://img.shields.io/badge/README-Русский-5277C3.svg" alt="Русский README">
  </a>
  <a href="https://tangled.org/zumuvik.tngl.sh/lavis/pipelines">
    <img src="https://img.shields.io/badge/CI-Tangled-5A67D8.svg" alt="Tangled CI">
  </a>
  <a href="https://tangled.org/zumuvik.tngl.sh/lavis/blob/main/LICENSE">
    <img src="https://img.shields.io/badge/license-GPL--3.0--only-blue.svg" alt="GPL-3.0-only">
  </a>
  <img src="https://img.shields.io/badge/Rust-stable-orange.svg" alt="Rust">
  <img src="https://img.shields.io/badge/NixOS-supported-5277C3.svg" alt="NixOS">
  <img src="https://img.shields.io/badge/status-alpha-yellow.svg" alt="Alpha">
</p>

</div>

---

## What is Lavis

Lavis is a personal Telegram userbot built in Rust on top of [grammers](https://github.com/Lonami/grammers).

It works through your Telegram account, handles commands from your own messages, edits the original command message with the result, and supports both built-in commands and external modules.

```text
,ping
,stats
,fastfetch
,help
```

The default prefix is `,`, but it can be changed at runtime.

---

## Features

| Feature | Description |
| --- | --- |
| ⚡ **Native Rust core** | Async Telegram access directly over MTProto |
| 🔐 **Local authorization** | Telegram session and API credentials stay local with restrictive permissions |
| ✏️ **Message editing** | Command output replaces the original outgoing message |
| 🧩 **External modules** | Language-agnostic modules over a JSON Lines protocol |
| 📦 **`.lmod` packages** | Reviewed module installation through Saved Messages |
| 🔌 **Module API v6** | Capability-based events, Telegram RPC adapters and bounded raw MTProto access |
| 🐧 **NixOS integration** | Flake, package, dev shell and ready-to-use NixOS module |
| 🔧 **Prefixes and aliases** | Persistent aliases and configurable command prefix |
| 🤖 **Companion bot** | Optional BotFather-backed companion/workspace setup |
| 🖥️ **Fastfetch** | Fastfetch passthrough with user arguments |
| 🌐 **English / Russian UI** | Persistent interface language and onboarding |

---

## Quick start

### 1. Get Telegram API credentials

Create an application at [my.telegram.org/apps](https://my.telegram.org/apps) and obtain:

- `API ID`;
- `API hash`.

### 2. Run Lavis

```bash
nix run 'git+https://tangled.org/zumuvik.tngl.sh/lavis'
```

On first interactive launch Lavis stores the API credentials locally and asks for the Telegram phone number, login code and, when enabled, the two-factor password.

Authorization can also be started explicitly:

```bash
nix run 'git+https://tangled.org/zumuvik.tngl.sh/lavis' -- auth
```

After authorization, start the Telegram introduction from Saved Messages:

```text
,start
```

Choose the interface language explicitly if needed:

```text
,start en
,start ru
```

> [!IMPORTANT]
> Never publish your API hash, `credentials.json`, Telegram session database, companion token, XDG state directories, or authorization logs containing secrets.

---

## Main commands

| Command | Purpose |
| --- | --- |
| `start` | Start or continue onboarding |
| `language` | Show or change the interface language |
| `help` | Command, alias and module help |
| `modules` | Built-in and active external modules |
| `ping` | Live Telegram RPC latency |
| `stats` | Uptime, latency, process and host statistics |
| `prefix` | Show, set or reset the command prefix |
| `alias` | Manage persistent command aliases |
| `fastfetch` | Runs the system fastfetch with passed arguments |
| `setup` | Companion bot/workspace setup and repair |
| `lm` | Install and control external `.lmod` modules |
| `reboot` | Restart the Lavis process |

<details>
<summary><b>Command examples</b></summary>

```text
,help
,help fastfetch
,modules
,ping
,stats
,prefix .
.alias add sys fastfetch --logo arch
.sys
.lm list
```

</details>

---

## External modules

An external Lavis module is a separate executable communicating with the core through JSON Lines over `stdin` and `stdout`.

Modules can be written in Rust, Go, Python, or any other suitable language. The current Module API v6 supports capability-gated Telegram operations, message events and a bounded raw MTProto escape hatch.

The repository ships a reference external module, **Cleaner** (`modules/cleaner`): scheduled deferred cleanup of your own messages, per-group management, and an `opsec` scan that surfaces ghost chats still holding your messages — `,cleaner [list|add <n>|remove <n>|status|log|run|opsec [add <n>]]`.

Local module operations:

```bash
lavis modules validate ./my-module/module.json
lavis modules enable my-module
lavis modules disable my-module
lavis modules status
```

### Install a `.lmod` from Telegram

1. Attach a `.lmod` archive to a **new self-authored message in Saved Messages**.
2. Send:

   ```text
   ,lm install
   ```

3. Review the installation plan.
4. Confirm the one-time approval ID:

   ```text
   ,lm confirm XXXX-XXXX-XXXX-XXXX
   ```

5. Enable the module and restart Lavis:

   ```text
   ,lm enable <module-id>
   ,reboot
   ```

Useful commands include `,lm list`, `,lm info <id>`, `,lm logs <id>` and `,lm doctor [id]`.

> [!WARNING]
> External modules are not placed in a system sandbox and run with the Lavis user's OS permissions. Capability checks constrain core-mediated operations, not arbitrary OS access. Install only code you trust.

See [External modules](docs/external-modules.md), [`.lmod` packaging](docs/lmod-packaging.md), and [Module API v6](docs/module-api-v6.md).

---

## Authorization recovery

Lavis locks the local Telegram session and distinguishes recoverable authorization problems from deterministic session corruption.

Useful CLI commands:

```bash
lavis credentials
lavis credentials reset
lavis auth doctor
lavis auth reset --backup
lavis logout
```

`auth doctor` inspects the local session. `auth reset --backup` replaces a broken session while preserving a backup. `logout` removes only the local session and does not revoke Telegram sessions remotely.

---

## NixOS

Add Lavis to your flake inputs:

```nix
{
  inputs.lavis.url = "git+https://tangled.org/zumuvik.tngl.sh/lavis";
}
```

### Binary cache

Lavis publishes Nix build outputs to Cachix. Add the cache declaratively so Nix can download prebuilt Lavis packages instead of compiling them locally:

```nix
{
  nix.settings = {
    extra-substituters = [ "https://lavis.cachix.org" ];
    extra-trusted-public-keys = [
      "lavis.cachix.org-1:EXJoSAQxNZb8j/p/2DrBBLmOXHP0VemCUZ5FdifeHbg="
    ];
  };
}
```

Then rebuild your NixOS configuration normally. Future Lavis outputs that are available in the cache will be substituted from `lavis.cachix.org` instead of being built locally.

Import the module and enable the service:

```nix
{
  imports = [ inputs.lavis.nixosModules.default ];

  services.lavis = {
    enable = true;
    autoStart = false;
    credentialsEnvironmentFile = "/run/secrets/lavis.env";
  };
}
```

By default the module creates the dedicated `lavis` system user with home `/var/lib/lavis`. After applying the configuration, authorize through the supplied helper and start the service:

```bash
sudo lavis-auth
sudo systemctl start lavis.service
```

After successful authorization, `autoStart` can be enabled.

Declarative external modules are also supported:

```nix
services.lavis.extensions = [
  {
    id = "gaf";
    package = inputs.lavis.packages.${pkgs.system}.lavis-extension-gaf;
  }
];
```

See [NixOS module](docs/nixos-module.md) for service, recovery and declarative extension details.

---

## Any other Linux

The same flake ships `x86_64-linux` and `aarch64-linux` outputs, so Lavis runs on
any Linux distribution with Nix and flakes enabled — no NixOS required. The binary
package is fully self-contained in the Nix store; `fastfetch` is already wired into
the wrapper's `PATH`.

Add the binary cache to `~/.config/nix/nix.conf` so future updates are substituted
instead of compiled locally (aarch64 outputs are usually not cached yet):

```ini
extra-substituters = https://lavis.cachix.org
extra-trusted-public-keys = lavis.cachix.org-1:EXJoSAQxNZb8j/p/2DrBBLmOXHP0VemCUZ5FdifeHbg=
```

Install and authorize once interactively:

```bash
nix profile install 'git+https://tangled.org/zumuvik.tngl.sh/lavis'
lavis auth
```

Run it as a systemd user service in `~/.config/systemd/user/lavis.service`:

```ini
[Unit]
Description=Lavis Telegram userbot
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=%h/.nix-profile/bin/lavis run
WorkingDirectory=%h
Environment=LAVIS_SERVICE=1
Restart=on-failure
# Status 78 means interactive reauthorization is required; retrying cannot fix it.
RestartPreventExitStatus=78
RestartSec=5s

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now lavis.service
loginctl enable-linger "$USER"
```

Update with `nix profile upgrade '.*'` followed by
`systemctl --user restart lavis.service`; rollback is `nix profile rollback`.
State lives in the XDG directories below and survives updates.

---

## Local data

Lavis keeps mutable data outside the Nix store under XDG paths:

```text
$XDG_CONFIG_HOME/lavis/       # credentials, companion token
$XDG_STATE_HOME/lavis/        # MTProto session, settings, aliases and runtime state
$XDG_DATA_HOME/lavis/         # installed modules and staging data
```

Without explicit XDG variables these resolve under `~/.config`, `~/.local/state` and `~/.local/share`.

---

## Development

```bash
git clone https://tangled.org/zumuvik.tngl.sh/lavis
cd lavis
nix develop
cargo run
```

Project checks:

```bash
cargo fmt --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
nix flake check --print-build-logs
nix build --print-build-logs
```

---

## Documentation

| Document | Scope |
| --- | --- |
| [Companion bot setup](docs/companion-bot-setup.md) | BotFather flow, workspace resources and recovery |
| [External modules](docs/external-modules.md) | External module lifecycle and runtime model |
| [`.lmod` packaging](docs/lmod-packaging.md) | Packaging and validation rules |
| [Module API v1](docs/module-api-v1.md) | Early module metadata/API |
| [Module API v2](docs/module-api-v2.md) / [v3](docs/module-api-v3.md) | Manifest and event protocol evolution |
| [Module API v4](docs/module-api-v4.md) | Message edits and reaction sets |
| [Module API v5](docs/module-api-v5.md) | Telegram account-status gateway |
| [Module API v6](docs/module-api-v6.md) | Capability-based Telegram RPC and raw MTProto boundary |
| [NixOS module](docs/nixos-module.md) | Declarative service configuration |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development and contribution guide |

---

## Security

Lavis has access to the authenticated Telegram session.

- do not run untrusted Lavis builds;
- do not install unknown external modules;
- do not publish API credentials or session files;
- do not share Lavis XDG directories;
- prefer `RUST_LOG=lavis=debug` over globally enabling dependency debug logs;
- remember that Fastfetch may expose host information.

---

## Project status

Lavis is under active development and is currently **alpha software**. Module APIs, `.lmod` format, persistence schema, commands and Nix interfaces may change incompatibly.

Lavis is an unofficial Telegram client. Userbot usage may lead to account restrictions or loss. You are responsible for your Telegram account, installed modules, credentials and compliance with Telegram rules and applicable law.

---

## License

Copyright © 2026 zumuvik.

Lavis is licensed under [GNU GPL-3.0-only](LICENSE).
