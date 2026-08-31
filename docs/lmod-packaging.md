# Packaging `.lmod` archives

A `.lmod` file is a restricted ZIP container used for inspected installation of external Lavis modules. The format is intentionally narrower than general ZIP support so every accepted entry can be validated before installation.

## Required archive layout

Correct:

```text
module.json
bin/
bin/my-module
assets/
assets/example.txt
```

Incorrect:

```text
my-module/module.json
my-module/bin/my-module
```

`module.json` must appear exactly once at archive root. Its `id` becomes the target directory name under `$XDG_DATA_HOME/lavis/modules/`.

## Build the module directory

```bash
mkdir -p my-module/bin
chmod 0700 my-module
chmod 0644 my-module/module.json
chmod 0755 my-module/bin/my-module
```

The manifest, module directory and entrypoint must not be group- or world-writable. The entrypoint must be a regular executable file and stay below the module root.

Validate before packaging:

```bash
lavis modules validate ./my-module/module.json
```

The archive may contain a schema 2, 3, 4, 5, or 6 manifest. Package the selected
schema unchanged; V5 gateway fields are documented in [Module API v5](module-api-v5.md),
and V6 raw-Telegram fields in [Module API v6](module-api-v6.md).

## Create a compatible archive

Lavis currently accepts only unencrypted ZIP entries using method 0 (**stored**, no compression).

With Info-ZIP:

```bash
cd my-module
zip -0 -X -r ../my-module.lmod module.json bin assets
cd ..
```

Omit paths that do not exist. Do not run `zip` from the parent as `zip -r my-module.lmod my-module`, because that adds an extra top-level directory.

The filename must end with lowercase `.lmod`.

## Verify the archive

```bash
unzip -l my-module.lmod
zipinfo -l my-module.lmod
```

Verify that:

- `module.json` is listed at root;
- the entrypoint path matches `module.json`;
- the entrypoint retains executable bits;
- compression method is stored;
- no symlink or special-file entries exist;
- no absolute, parent (`..`), dot (`.`), backslash or Windows drive-prefix paths exist.

## Current hard limits

| Limit | Value |
| --- | ---: |
| Archive bytes | 16 MiB |
| Archive entries | 256 |
| Expanded bytes per file | 4 MiB |
| Total expanded bytes | 32 MiB |
| Path depth | 16 components |
| Path length | 1024 bytes |
| Manifest size | 64 KiB |
| Commands per module | 32 |

The Telegram download applies the 16 MiB boundary to both declared size, when present, and actual received bytes. Download stops as soon as the actual boundary is exceeded.

Because only stored entries are accepted, compressed and expanded bytes normally match. Encryption, data formats or compression methods that Lavis cannot verify are rejected.

## Rejected entry types and paths

Lavis rejects:

- symlinks;
- device nodes;
- FIFOs and sockets;
- setuid, setgid or sticky permission bits;
- duplicate paths;
- a file that conflicts with a descendant path;
- absolute paths;
- `.` or `..` components;
- backslashes;
- Windows drive prefixes such as `C:`;
- NUL bytes;
- paths deeper or longer than the configured limits;
- more than one root `module.json`;
- encrypted or compressed entries.

## Installation flow

Attach the archive to a new own message in Saved Messages:

```text
,lm install
```

Review the plan and confirm the full ApprovalId within 10 minutes:

```text
,lm confirm XXXX-XXXX-XXXX-XXXX
```

The plan shows the exact archive SHA-256 and a canonical plan fingerprint. Repacking identical files may change the archive digest because ZIP metadata and byte layout can differ.

On confirmation Lavis uses atomic no-replace rename semantics. It never overwrites an existing module directory and does not fall back to copying across filesystems. The manifest is validated again from the final installed path. The module remains disabled after installation.

Enable and restart:

```bash
lavis modules enable <module-id>
```

## What `.lmod` does not provide

The format currently has no:

- digital signature;
- publisher identity or trust store;
- dependency declaration/build step;
- remote repository metadata;
- update or uninstall transaction;
- sandbox guarantee;
- reproducible-build guarantee.

Inspection confirms that the package obeys Lavis' structural and manifest rules. It does not prove that the executable is safe.
