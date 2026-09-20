# Remote Fastfetch

`remotefetch` is an external Lavis Module API v6 module that answers `,remotefetch` with
fastfetch output from **another machine** — a laptop or workstation — instead
of the server the userbot runs on. It is distributed as `remotefetch.lmod`.

It has two parts:

| Part | Runs on | What it does |
| --- | --- | --- |
| `remotefetch` | the Lavis host (server) | the Lavis module; asks the agent and renders the reply |
| `lavis-fetchd` | the remote machine | runs fastfetch locally and returns its output over HTTP |

```text
server (lavis)                        laptop (lavis-fetchd)
  ,remotefetch --logo NixOS
        │  POST /v1/fastfetch  Bearer <token>
        │  {"args":["--logo","NixOS"]}
        ▼
                                      fastfetch --config none --pipe --logo NixOS
        ▲
        │  200 {"output":"…","host":"xpert","took_ms":84}
   the command message is edited with the result
```

The userbot never gets shell access to that machine: it can ask for fastfetch
output and nothing else.

## Telegram commands

- `,remotefetch` or `,remotefetch.fetch` — fastfetch from the laptop;
- `,remotefetch --logo none --structure OS:Kernel:CPU:Memory` — any fastfetch option;
- `,remotefetch.status` — is the agent reachable, which fastfetch version it has, how
  long it has been up, and the round-trip latency.

Arguments are grouped the way a shell would group them, so
`,remotefetch --separator " -> "` arrives as one argument. No shell runs at any point:
tokens travel to the agent as a JSON array and reach `execve` unchanged, so
metacharacters stay data.

`,remotefetch.status` is the command to reach for when something is wrong — it
separates a sleeping laptop from a wrong token or a wrong address.

## Argument policy

Every fastfetch option is passed through except the ones that read or write a
path of the caller's choosing: `--file`, `--file-raw`, `--raw`, `--config`,
`--gen-config`, `--logo-type`, `--sixel`, `--kitty*`, `--iterm`, `--chafa`, and
`--logo` with a value that looks like a path. Without that list,
`,remotefetch --file ~/.ssh/id_ed25519` would render a private key as the logo.

The agent enforces the policy; the module does not pre-check. A compromised or
outdated module therefore cannot widen what that machine will run. Start the
agent with `--allow-file-args` to lift the restriction.

The rest of the fastfetch surface is intentionally unrestricted, so output can
still reveal host, network, display, power and hardware data about the laptop
— the same exposure the built-in `,fastfetch` has for the Lavis host.

## Agent setup on the remote machine

Generate a token and keep it readable only by the account running the agent:

```bash
install -m 600 /dev/null ~/.config/lavis-fetchd-token
head -c 32 /dev/urandom | base64 > ~/.config/lavis-fetchd-token
```

Run the agent on a private network address — a Tailscale or WireGuard address,
never a public one. It has no transport encryption of its own:

```bash
lavis-fetchd \
  --listen 100.120.95.96:8471 \
  --token-file ~/.config/lavis-fetchd-token
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--listen` | `127.0.0.1:8471` | listening address |
| `--token-file` | — | file holding the shared bearer token (required) |
| `--timeout` | `2.5s` | maximum fastfetch run time |
| `--allow-file-args` | `false` | permit the file-reading fastfetch options |

The agent refuses to start when the token file is world-readable or holds
fewer than 16 bytes. Both endpoints require the token: an unauthenticated
health probe would hand out the hostname and fastfetch version to anything that
can reach the listener.

Run it as a **user** service, not a system service: fastfetch reads the session
environment to detect the desktop, window manager, terminal and displays, so a
root service reports the hardware correctly and leaves those fields empty. The
NixOS module below does this.

### NixOS

```nix
{
  imports = [ inputs.lavis.nixosModules.fetchd ];

  services.lavis-fetchd = {
    enable = true;
    user = "wardar";
    listenAddress = "100.120.95.96:8471";     # this machine on the tailnet
    tokenFile = "/run/secrets/lavis-fetchd-token";
  };
}
```

Do not put the token in the Nix store — every store path is world-readable.
Use agenix, sops-nix, or a file placed outside the store.

The module enables lingering for that user so the agent keeps answering while
nobody is logged in. Set `openFirewall = true` if the tailnet interface is not
already covered by `networking.firewall.trustedInterfaces`.

## Module setup on the Lavis host

The module reads `config.json` from `LAVIS_MODULE_STATE_DIR` — Lavis starts
modules with a cleared environment, so this is the only path it can rely on:

```bash
install -d -m 700 /var/lib/lavis/.local/state/lavis/modules/remotefetch
install -m 600 /dev/null /var/lib/lavis/.local/state/lavis/modules/remotefetch/config.json
```

```json
{
  "url": "http://100.120.95.96:8471",
  "token": "the same token as the agent",
  "label": "xpert"
}
```

| Field | Meaning |
| --- | --- |
| `url` | agent address, `http://host:port` |
| `token` | shared bearer token |
| `token_file` | read the token from this path instead of inlining it |
| `label` | name used in replies; defaults to the hostname the agent reports |

When run outside Lavis the module also accepts
`~/.config/lavis/remotefetch.json`.

## Build, test and install

```bash
cd modules/remotefetch
go test ./...
go vet ./...
./build-lmod.sh
```

Send `dist/remotefetch.lmod` to Saved Messages in a new message with:

```text
,lm install
```

Review the inspection plan and confirm its full approval ID within ten minutes:

```text
,lm confirm XXXX-XXXX-XXXX-XXXX
```

The installed module remains disabled. Enable it and restart Lavis:

```bash
lavis modules enable remotefetch
```

On NixOS, install it declaratively instead:

```nix
services.lavis.extensions = [
  {
    id = "remotefetch";
    package = inputs.lavis.packages.${pkgs.system}.lavis-extension-remotefetch;
  }
];
```

## Capabilities

`network` to reach the agent, and `persistent_state_read` to read
`config.json`. The module holds no Telegram capabilities: it sends no
messages, reads none, and makes no Telegram RPC — it answers a command with
text and nothing else.

## Fidelity

Running as a user service gets the session-dependent fields right — desktop,
window manager, displays, theme. Two fields still differ from what an
interactive run shows, because fastfetch derives them from its own process
tree: `Shell` names `lavis-fetchd` rather than your login shell, and `Terminal`
names the agent too. Exclude them when it matters:

```text
,remotefetch --structure OS:Host:Kernel:Uptime:Packages:WM:CPU:GPU:Memory:Disk
```

## Timing

The Module API v6 lifecycle deadline is five seconds. The budget nests inside
it: the command gets 4s, the HTTP request 3.5s, and the agent's fastfetch run
2.5s. A laptop that is asleep or off the network fails on connect and answers
in milliseconds rather than burning the deadline.
