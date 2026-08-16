{ self }:

{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    concatMapStringsSep
    literalExpression
    mkEnableOption
    mkIf
    mkOption
    optional
    optionalString
    types
    ;

  cfg = config.services.lavis;
  system = pkgs.stdenv.hostPlatform.system;
  extensionLib = import ../lib/extensions.nix { inherit pkgs; };

  serviceUser = cfg.user;
  createsDefaultUser = serviceUser == "lavis";

  declaredUser =
    if builtins.hasAttr serviceUser config.users.users then
      config.users.users.${cfg.user}
    else
      null;
  declaredUserGroup = if declaredUser != null then declaredUser.group else null;
  serviceGroup =
    if cfg.group != null then
      cfg.group
    else if createsDefaultUser then
      "lavis"
    else
      declaredUserGroup;
  declaredUserHome =
    if declaredUser != null then declaredUser.home else null;
  effectiveHome =
    if cfg.home != null then
      cfg.home
    else if createsDefaultUser then
      "/var/lib/lavis"
    else if declaredUserHome != null then
      declaredUserHome
    else
      "/var/lib/lavis";

  configHome = "${effectiveHome}/.config";
  stateHome = "${effectiveHome}/.local/state";
  dataHome = "${effectiveHome}/.local/share";
  lavisConfigDir = "${configHome}/lavis";
  lavisStateDir = "${stateHome}/lavis";
  lavisDataDir = "${dataHome}/lavis";
  modulesDir = "${lavisDataDir}/modules";
  moduleStateRoot = "${lavisStateDir}/modules";
  moduleStagingDir = "${lavisDataDir}/module-staging";
  declarativeStateFile = "${lavisStateDir}/declarative-modules.json";
  moduleIdType = types.addCheck types.str (
    id: builtins.match "[a-z][a-z0-9-]{0,31}" id != null
  );

  fastfetchProfileFile =
    if cfg.fastfetchProfile == null then
      null
    else
      pkgs.writeText "lavis-fastfetch.json" (builtins.toJSON cfg.fastfetchProfile);

  extensionModule =
    { ... }:
    {
      options = {
        id = mkOption {
          type = moduleIdType;
          description = "Lavis external module id. Must match the module manifest id.";
          example = "gaf";
        };

        package = mkOption {
          type = types.nullOr (types.either types.package types.path);
          default = null;
          description = "Directory package containing module.json and the module entrypoint.";
        };

        url = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "URL of a .lmod archive to fetch and install declaratively.";
          example = "https://example.invalid/my-module.lmod";
        };

        hash = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "Hash for the .lmod archive declared by url.";
          example = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        };

        enable = mkOption {
          type = types.bool;
          default = true;
          description = "Whether this declarative extension id should be enabled in Lavis state.";
        };
      };
    };

  extensionSource =
    ext:
    if ext.package != null then
      ext.package
    else
      extensionLib.buildLavisExtensionFromLmod {
        inherit (ext) id;
        src = pkgs.fetchurl {
          inherit (ext) url hash;
        };
      };

  extensions = map (ext: ext // { source = extensionSource ext; }) cfg.extensions;
  declarativeIdsJson = pkgs.writeText "lavis-declarative-extension-ids.json" (
    builtins.toJSON (map (ext: ext.id) extensions)
  );
  enabledIdsJson = pkgs.writeText "lavis-enabled-extension-ids.json" (
    builtins.toJSON (map (ext: ext.id) (builtins.filter (ext: ext.enable) extensions))
  );

  installExtensionCommands = concatMapStringsSep "\n" (
    ext: ''
      install_lavis_extension ${lib.escapeShellArg ext.id} ${lib.escapeShellArg (toString ext.source)}
    ''
  ) extensions;

  setupScript = pkgs.writeShellScript "lavis-setup" ''
    set -euo pipefail

    install -d -m 700 \
      ${lib.escapeShellArg lavisConfigDir} \
      ${lib.escapeShellArg lavisStateDir} \
      ${lib.escapeShellArg lavisDataDir} \
      ${lib.escapeShellArg modulesDir} \
      ${lib.escapeShellArg moduleStagingDir}

    ${lib.optionalString (cfg.settings.prefix != null) ''
      ${pkgs.python3}/bin/python3 - ${lib.escapeShellArg "${lavisStateDir}/settings.json"} ${lib.escapeShellArg cfg.settings.prefix} <<'PY'
import json
import os
import stat
import sys
import tempfile

path, prefix = sys.argv[1:]
parent = os.path.dirname(path)
allowed_progress = {
    "not_started", "intro", "prefix", "ping", "help", "modules",
    "external_modules", "companion", "completion", "complete", "skipped",
}

def valid_prefix(value):
    return (isinstance(value, str) and 1 <= len(value) <= 4
            and not any(ch.isspace() or ch.isalpha() or ord(ch) < 32 or ord(ch) == 127
                        or __import__("unicodedata").category(ch) == "Cf" for ch in value))

def checked_file(candidate):
    info = os.lstat(candidate)
    if not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o600:
        raise SystemExit("lavis settings file is not a private regular file")

if not valid_prefix(prefix):
    raise SystemExit("services.lavis.settings.prefix is invalid")

try:
    checked_file(path)
except FileNotFoundError:
    state = {"version": 2, "prefix": prefix, "locale": None, "onboarding": "not_started"}
else:
    try:
        with open(path, "r", encoding="utf-8") as handle:
            previous = json.load(handle)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SystemExit("could not read existing Lavis settings safely") from error
    if not isinstance(previous, dict):
        raise SystemExit("existing Lavis settings are malformed")
    version = previous.get("version")
    if version == 1 and set(previous) == {"version", "prefix"} and valid_prefix(previous["prefix"]):
        state = {"version": 2, "prefix": prefix, "locale": None, "onboarding": "not_started"}
    elif (version == 2 and set(previous) == {"version", "prefix", "locale", "onboarding"}
          and valid_prefix(previous["prefix"])
          and previous["locale"] in (None, "en", "ru", "english", "russian")
          and previous["onboarding"] in allowed_progress):
        # Preserve user-owned v2 state. Legacy locale spellings are accepted by
        # the Rust migration and normalized on its next write.
        locale = {"english": "en", "russian": "ru"}.get(previous["locale"], previous["locale"])
        state = {"version": 2, "prefix": prefix, "locale": locale, "onboarding": previous["onboarding"]}
    else:
        raise SystemExit("existing Lavis settings have an unsupported schema")

payload = (json.dumps(state, indent=2) + "\n").encode()
fd, temporary = tempfile.mkstemp(prefix=".settings.json.nixos.", dir=parent)
try:
    os.fchmod(fd, 0o600)
    with os.fdopen(fd, "wb", closefd=True) as handle:
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)
    directory = os.open(parent, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)
except BaseException:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    raise
PY
    ''}

    ${lib.optionalString (fastfetchProfileFile != null) ''
      install -m 600 \
        ${lib.escapeShellArg fastfetchProfileFile} ${lib.escapeShellArg "${lavisConfigDir}/fastfetch.json"}
    ''}

    install_lavis_extension() {
      local id="$1"
      local src="$2"
      local dest=${lib.escapeShellArg modulesDir}/"$id"
      local state_dir=${lib.escapeShellArg moduleStateRoot}/"$id"
      local staging
      local old

      if [ ! -f "$src/module.json" ]; then
        echo "lavis extension $id: $src/module.json does not exist" >&2
        exit 1
      fi
      if ! ${pkgs.python3}/bin/python3 - "$id" "$src/module.json" <<'PY'
import json
import sys

expected_id, manifest_path = sys.argv[1:3]
with open(manifest_path, "r", encoding="utf-8") as handle:
    manifest = json.load(handle)
actual_id = manifest.get("id")
if actual_id != expected_id:
    raise SystemExit(
        f"lavis extension {expected_id}: module.json id {actual_id!r} does not match declarative id"
    )
PY
      then
        exit 1
      fi

      install -d -m 700 "$state_dir"

      staging=$(mktemp -d -p ${lib.escapeShellArg moduleStagingDir} ".nixos-$id.XXXXXXXXXX")
      trap 'rm -rf "$staging"' RETURN

      cp -R --no-dereference --no-preserve=ownership "$src/." "$staging/"
      chmod -R u+rwX,go-rwx "$staging"
      printf '%s\n' 'managed-by=services.lavis' > "$staging/.lavis-nixos-module"
      chmod 600 "$staging/.lavis-nixos-module"

      if [ -e "$dest" ] || [ -L "$dest" ]; then
        old="$staging.old"
        mv -T "$dest" "$old"
        if [ -f "$old/state.json" ] && [ ! -L "$old/state.json" ] && [ ! -e "$state_dir/state.json" ]; then
          install -m 600 "$old/state.json" "$state_dir/state.json"
        fi
        mv -T "$staging" "$dest"
        rm -rf "$old"
      else
        mv -T "$staging" "$dest"
      fi
      trap - RETURN
    }

    ${installExtensionCommands}

    ${pkgs.python3}/bin/python3 ${./merge-enabled-extensions.py} \
      ${lib.escapeShellArg "${lavisStateDir}/external-modules.json"} \
      ${lib.escapeShellArg declarativeStateFile} \
      ${lib.escapeShellArg declarativeIdsJson} \
      ${lib.escapeShellArg enabledIdsJson}
  '';

  authSetupScript = pkgs.writeShellScript "lavis-auth-setup" ''
    set -euo pipefail

    ${optionalString createsDefaultUser ''
    ${pkgs.coreutils}/bin/mkdir -p \
      ${lib.escapeShellArg effectiveHome}

    ${pkgs.coreutils}/bin/chmod 700 \
      ${lib.escapeShellArg effectiveHome}
    ''}

    ${pkgs.coreutils}/bin/mkdir -p \
      ${lib.escapeShellArg lavisConfigDir} \
      ${lib.escapeShellArg lavisStateDir} \
      ${lib.escapeShellArg lavisDataDir}

    ${pkgs.coreutils}/bin/chmod 700 \
      ${lib.escapeShellArg lavisConfigDir} \
      ${lib.escapeShellArg lavisStateDir} \
      ${lib.escapeShellArg lavisDataDir}

    exec ${cfg.package}/bin/lavis auth
  '';

  authScript = pkgs.writeShellScriptBin "lavis-auth" ''
    set -euo pipefail

    if [ "$(${pkgs.coreutils}/bin/id -u)" != 0 ]; then
      echo "lavis-auth must be run as root so it can read the service credential file and switch to ${serviceUser}." >&2
      exit 1
    fi

    credential_env=()
    ${optionalString (cfg.credentialsEnvironmentFile != null) ''
      credential_output="$(${pkgs.python3}/bin/python3 - ${lib.escapeShellArg cfg.credentialsEnvironmentFile} <<'PY'
import re
import sys

path = sys.argv[1]
allowed = {"LAVIS_API_ID", "LAVIS_API_HASH"}
values = {}
line_re = re.compile(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*([^#\s]+)\s*(?:#.*)?$")

with open(path, "r", encoding="utf-8") as handle:
    for number, line in enumerate(handle, 1):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        match = line_re.match(line)
        if match is None or match.group(1) not in allowed:
            raise SystemExit(f"{path}:{number}: expected literal LAVIS_API_ID=... or LAVIS_API_HASH=...")
        key, value = match.groups()
        if any(ord(ch) < 32 or ord(ch) == 127 for ch in value):
            raise SystemExit(f"{path}:{number}: invalid control character")
        values[key] = value

api_id = values.get("LAVIS_API_ID")
api_hash = values.get("LAVIS_API_HASH")
if (api_id is None) != (api_hash is None):
    raise SystemExit(f"{path}: LAVIS_API_ID and LAVIS_API_HASH must be set together")
if api_id is not None:
    if not api_id.isdigit():
        raise SystemExit(f"{path}: LAVIS_API_ID must be decimal digits")
    if re.fullmatch(r"[A-Fa-f0-9]{32}", api_hash) is None:
        raise SystemExit(f"{path}: LAVIS_API_HASH must be 32 hexadecimal characters")
    print(f"LAVIS_API_ID={api_id}")
    print(f"LAVIS_API_HASH={api_hash}")
PY
      )"
      if [ -n "$credential_output" ]; then
        mapfile -t credential_env <<< "$credential_output"
      fi
    ''}

    ${optionalString createsDefaultUser ''
      ${pkgs.coreutils}/bin/install -d -m 700 -o ${lib.escapeShellArg serviceUser} -g ${lib.escapeShellArg serviceGroup} \
        ${lib.escapeShellArg effectiveHome}
    ''}

    lavis_env=(
      ${lib.escapeShellArg "HOME=${effectiveHome}"}
      ${lib.escapeShellArg "XDG_CONFIG_HOME=${configHome}"}
      ${lib.escapeShellArg "XDG_STATE_HOME=${stateHome}"}
      ${lib.escapeShellArg "XDG_DATA_HOME=${dataHome}"}
      ${lib.escapeShellArg "RUST_LOG=${cfg.logLevel}"}
      "''${credential_env[@]}"
    )

    exec ${pkgs.util-linux}/bin/runuser \
      --user ${lib.escapeShellArg serviceUser} \
      --group ${lib.escapeShellArg serviceGroup} \
      -- \
      ${pkgs.coreutils}/bin/env \
        -i \
        "''${lavis_env[@]}" \
        ${authSetupScript}
  '';
in
{
  options.services.lavis = {
    enable = mkEnableOption "Lavis Telegram userbot";

    package = mkOption {
      type = types.package;
      default = self.packages.${system}.default;
      defaultText = literalExpression "inputs.lavis.packages.${pkgs.stdenv.hostPlatform.system}.default";
      description = "Lavis package to run.";
    };

    user = mkOption {
      type = types.str;
      default = "lavis";
      description = "Unix user that owns and runs Lavis. The default creates a dedicated system user.";
      example = "lavis";
    };

    group = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Unix group for Lavis files. Defaults to the declared primary group of services.lavis.user or to the dedicated lavis group.";
      example = "lavis";
    };

    home = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Home directory used to derive Lavis XDG paths. Defaults to the configured user's home or /var/lib/lavis.";
      example = "/var/lib/lavis";
    };

    autoStart = mkOption {
      type = types.bool;
      default = true;
      description = "Whether lavis.service should start at boot.";
    };

    credentialsEnvironmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Environment file containing LAVIS_API_ID and LAVIS_API_HASH.";
      example = "/run/secrets/lavis.env";
    };

    logLevel = mkOption {
      type = types.str;
      default = "info";
      description = "RUST_LOG value for the Lavis service.";
    };

    settings.prefix = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Optional declarative command prefix written to Lavis settings.json.";
      example = ",";
    };

    fastfetchProfile = mkOption {
      type = types.nullOr types.attrs;
      default = null;
      description = "Optional fastfetch profile written to Lavis fastfetch.json.";
      example = literalExpression ''
        {
          version = 1;
          logo = "NixOS";
          structure = [ "title" "os" "kernel" ];
        }
      '';
    };

    extensions = mkOption {
      type = types.listOf (types.submodule extensionModule);
      default = [ ];
      description = "Declarative Lavis external modules to install from packages or fetched .lmod archives.";
      example = literalExpression ''
        [
          { id = "gaf"; package = inputs.lavis.packages.''${pkgs.system}.lavis-extension-gaf; }
          {
            id = "my-module";
            url = "https://example.invalid/my-module.lmod";
            hash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
          }
        ]
      '';
    };
  };

  config = mkIf cfg.enable {
    assertions =
      [
        {
          assertion = cfg.home != null || createsDefaultUser || declaredUserHome != null;
          message = "services.lavis.home must be set when services.lavis.user has no declared home.";
        }
        {
          assertion = cfg.group != null || createsDefaultUser || declaredUserGroup != null;
          message = "services.lavis.group must be set when services.lavis.user has no declared primary group.";
        }
      ]
      ++ map (ext: {
        assertion = (ext.package != null) != (ext.url != null);
        message = "services.lavis.extensions entry ${ext.id} must set exactly one of package or url.";
      }) cfg.extensions
      ++ map (ext: {
        assertion = ext.url == null || ext.hash != null;
        message = "services.lavis.extensions entry ${ext.id} with url must also set hash.";
      }) cfg.extensions;

    users.groups = mkIf createsDefaultUser {
      ${serviceGroup} = { };
    };

    users.users = mkIf createsDefaultUser {
      ${serviceUser} = {
        isSystemUser = true;
        group = serviceGroup;
        home = effectiveHome;
        createHome = true;
        homeMode = "0700";
      };
    };

    systemd.tmpfiles.rules = optional createsDefaultUser (
      "d ${effectiveHome} 0700 ${serviceUser} ${serviceGroup} - -"
    );

    environment.systemPackages = [ authScript ];

    systemd.services.lavis = {
      description = "Lavis Telegram userbot";
      wantedBy = optional cfg.autoStart "multi-user.target";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      environment = {
        HOME = effectiveHome;
        XDG_CONFIG_HOME = configHome;
        XDG_STATE_HOME = stateHome;
        XDG_DATA_HOME = dataHome;
        RUST_LOG = cfg.logLevel;
        LAVIS_SERVICE = "1";
      };

      preStart = "${setupScript}";

      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/lavis run";
        User = serviceUser;
        Group = serviceGroup;
        WorkingDirectory = effectiveHome;
        Restart = "on-failure";
        # Status 78 means local interactive reauthorization is required; retrying
        # cannot recover it and would only create a restart loop.
        RestartPreventExitStatus = "78";
        RestartSec = "5s";
        EnvironmentFile = optional (cfg.credentialsEnvironmentFile != null) cfg.credentialsEnvironmentFile;
      };
    };
  };
}
