# NixOS module

Lavis exposes `nixosModules.default` for running the userbot as a NixOS
systemd service.

The module is intentionally conservative:

- it creates a dedicated `lavis` system user by default;
- it stores mutable data under that user's XDG directories;
- it never writes Telegram credentials or sessions into the Nix store;
- it installs external modules as real writable directories, not symlinks;
- it stages and atomically replaces declarative extension payloads as the Lavis
  service user.

## Basic setup

Add the Lavis flake input and import the module:

```nix
{
  inputs.lavis.url = "git+ssh://git@tangled.org/did:plc:xhzbac5le4gwflk4t6stjjgf";

  outputs = { self, nixpkgs, lavis, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        lavis.nixosModules.default
        {
          services.lavis = {
            enable = true;
            # Keep the service stopped until the first interactive auth
            # has created the Telegram session.
            autoStart = false;
            credentialsEnvironmentFile = "/run/secrets/lavis.env";
          };
        }
      ];
    };
  };
}
```

This creates a system user/group named `lavis` and stores mutable state under
`/var/lib/lavis`. To run as an existing account instead, set `user`. If that
user is not declared in the same NixOS configuration, set `group` and `home`
explicitly:

```nix
services.lavis = {
  enable = true;
  user = "melvi";
  group = "users";
  home = "/home/melvi";
};
```

By default the service starts at boot. Keep `autoStart = false` for first-time
authorization, then remove it or set it to `true` after `lavis-auth` succeeds.

## Credentials and first authorization

`credentialsEnvironmentFile` points to a root-managed file outside the Nix
store:

```text
LAVIS_API_ID=123456
LAVIS_API_HASH=your-api-hash
```

Use your normal NixOS secret manager for this file, for example agenix,
sops-nix, or another `/run/secrets/...` provider. Do not use `environment.etc`,
inline Nix strings, or committed files for the API hash.

Before starting the long-running service for the first time, authorize Telegram
interactively with the helper installed by the module:

```bash
sudo lavis-auth
```

`lavis-auth` parses only literal `LAVIS_API_ID` and `LAVIS_API_HASH` assignments
from `credentialsEnvironmentFile` as root, switches to the service user, creates
the Lavis XDG directories with private permissions, and runs `lavis auth` with
the same environment as `lavis.service`.

The default service uses these paths:

```text
/var/lib/lavis/.config/lavis/
/var/lib/lavis/.local/state/lavis/
/var/lib/lavis/.local/share/lavis/
```

The Telegram session remains mutable secret state under
`$XDG_STATE_HOME/lavis/session`.

On a fresh VPS, the usual sequence is:

```bash
nixos-rebuild test --flake .#host
sudo lavis-auth
sudo systemctl start lavis.service
# Remove autoStart = false, or set autoStart = true.
nixos-rebuild switch --flake .#host
```

## Settings

Set the command prefix declaratively:

```nix
services.lavis.settings.prefix = ",";
```

This writes:

```text
$XDG_STATE_HOME/lavis/settings.json
```

Set a Fastfetch profile declaratively:

```nix
services.lavis.fastfetchProfile = {
  version = 1;
  logo = "NixOS";
  structure = [ "title" "os" "kernel" "cpu" "memory" ];
  separator = " -> ";
};
```

This writes:

```text
$XDG_CONFIG_HOME/lavis/fastfetch.json
```

The values must match the schema accepted by Lavis at runtime. See the
Fastfetch section in [README.md](../README.md#fastfetch).

## Declarative extensions

Extensions can be installed from package/path outputs:

```nix
services.lavis.extensions = [
  {
    id = "gaf";
    package = lavis.packages.x86_64-linux.lavis-extension-gaf;
    enable = true;
  }
];
```

They can also be fetched as `.lmod` archives:

```nix
services.lavis.extensions = [
  {
    id = "my-module";
    url = "https://example.invalid/my-module.lmod";
    hash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    enable = true;
  }
];
```

Each extension must set exactly one of `package` or `url`. URL entries must set
`hash`.

Declarative extension payloads are copied into:

```text
$XDG_DATA_HOME/lavis/modules/<id>/
```

The copied directory is writable by the service user. On rebuild, Lavis' NixOS
module stages the declared payload as that user and atomically replaces the
previous declarative payload. Modules should keep mutable data under
the `LAVIS_MODULE_STATE_DIR` environment variable passed to the module process.
For the NixOS service this resolves to
`$XDG_STATE_HOME/lavis/modules/<id>/`, not next to the packaged entrypoint.

Enabled declarative extensions are merged into:

```text
$XDG_STATE_HOME/lavis/external-modules.json
```

User-enabled non-declarative modules are preserved. Declarative module IDs that
are removed from the NixOS config are removed from the declarative enabled set,
but their installed files are not deleted by the module.

Declarative extensions can be changed only through `services.lavis.extensions` and a NixOS
rebuild (for example, `nh os switch`). Telegram `,lm enable <module-id>` and
`,lm disable <module-id>` reject declaratively managed module IDs. Use `,lm list` and
`,lm info <module-id>` only to inspect them from Telegram.

External modules run with the Lavis service user's normal OS permissions. Only
declare extensions you trust.

## Complete example

```nix
{
  services.lavis = {
    enable = true;
    user = "melvi";
    credentialsEnvironmentFile = "/run/secrets/lavis.env";

    settings.prefix = ",";

    fastfetchProfile = {
      version = 1;
      logo = "NixOS";
      structure = [ "title" "os" "kernel" ];
    };

    extensions = [
      {
        id = "gaf";
        package = lavis.packages.x86_64-linux.lavis-extension-gaf;
      }
      {
        id = "my-module";
        url = "https://example.invalid/my-module.lmod";
        hash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
      }
    ];
  };
}
```

## Flake outputs

The flake provides:

```text
nixosModules.default
packages.x86_64-linux.default
packages.x86_64-linux.lavis-extension-gaf
lib.x86_64-linux.buildLavisExtensionFromLmod
```

`buildLavisExtensionFromLmod` accepts a fetched `.lmod` source and unpacks it
into a directory package:

```nix
let
  myExtension = lavis.lib.x86_64-linux.buildLavisExtensionFromLmod {
    id = "my-module";
    src = pkgs.fetchurl {
      url = "https://example.invalid/my-module.lmod";
      hash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    };
  };
in
{
  services.lavis.extensions = [
    { id = "my-module"; package = myExtension; }
  ];
}
```

## Useful commands

Check the generated service:

```bash
systemctl cat lavis.service
```

Follow logs:

```bash
journalctl -u lavis.service -f
```

Check discovered and enabled modules as the service user:

```bash
sudo -u melvi \
  XDG_CONFIG_HOME=/home/melvi/.config \
  XDG_STATE_HOME=/home/melvi/.local/state \
  XDG_DATA_HOME=/home/melvi/.local/share \
  lavis modules status
```
