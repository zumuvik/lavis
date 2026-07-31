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
    types
    ;

  cfg = config.services.lavis;
  system = pkgs.stdenv.hostPlatform.system;
  extensionLib = import ../lib/extensions.nix { inherit pkgs; };

  effectiveGroup = if cfg.group == null then cfg.user else cfg.group;
  declaredUserHome =
    if cfg.user != null && builtins.hasAttr cfg.user config.users.users then
      config.users.users.${cfg.user}.home
    else
      null;
  effectiveHome =
    if cfg.home != null then
      cfg.home
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

  settingsFile =
    if cfg.settings.prefix == null then
      null
    else
      pkgs.writeText "lavis-settings.json" (
        builtins.toJSON {
          version = 1;
          prefix = cfg.settings.prefix;
        }
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
          type = types.str;
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

    install -d -m 700 -o ${lib.escapeShellArg cfg.user} -g ${lib.escapeShellArg effectiveGroup} \
      ${lib.escapeShellArg lavisConfigDir} \
      ${lib.escapeShellArg lavisStateDir} \
      ${lib.escapeShellArg lavisDataDir} \
      ${lib.escapeShellArg modulesDir}

    ${lib.optionalString (settingsFile != null) ''
      install -m 600 -o ${lib.escapeShellArg cfg.user} -g ${lib.escapeShellArg effectiveGroup} \
        ${lib.escapeShellArg settingsFile} ${lib.escapeShellArg "${lavisStateDir}/settings.json"}
    ''}

    ${lib.optionalString (fastfetchProfileFile != null) ''
      install -m 600 -o ${lib.escapeShellArg cfg.user} -g ${lib.escapeShellArg effectiveGroup} \
        ${lib.escapeShellArg fastfetchProfileFile} ${lib.escapeShellArg "${lavisConfigDir}/fastfetch.json"}
    ''}

    install_lavis_extension() {
      local id="$1"
      local src="$2"
      local dest=${lib.escapeShellArg modulesDir}/"$id"

      if [ ! -f "$src/module.json" ]; then
        echo "lavis extension $id: $src/module.json does not exist" >&2
        exit 1
      fi

      install -d -m 700 -o ${lib.escapeShellArg cfg.user} -g ${lib.escapeShellArg effectiveGroup} "$dest"
      cp -R --dereference --no-preserve=ownership "$src/." "$dest/"
      chown -R ${lib.escapeShellArg cfg.user}:${lib.escapeShellArg effectiveGroup} "$dest"
      chmod -R u+rwX,go-rwx "$dest"
      printf '%s\n' 'managed-by=services.lavis' > "$dest/.lavis-nixos-module"
      chown ${lib.escapeShellArg cfg.user}:${lib.escapeShellArg effectiveGroup} "$dest/.lavis-nixos-module"
      chmod 600 "$dest/.lavis-nixos-module"
    }

    ${installExtensionCommands}

    ${lib.optionalString (extensions != [ ]) ''
      ${pkgs.python3}/bin/python3 ${./merge-enabled-extensions.py} \
        ${lib.escapeShellArg "${lavisStateDir}/external-modules.json"} \
        ${lib.escapeShellArg declarativeIdsJson} \
        ${lib.escapeShellArg enabledIdsJson}
      chown ${lib.escapeShellArg cfg.user}:${lib.escapeShellArg effectiveGroup} \
        ${lib.escapeShellArg "${lavisStateDir}/external-modules.json"}
      chmod 600 ${lib.escapeShellArg "${lavisStateDir}/external-modules.json"}
    ''}
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
      type = types.nullOr types.str;
      default = null;
      description = "Existing Unix user that owns and runs Lavis.";
      example = "melvi";
    };

    group = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Existing Unix group for Lavis files. Defaults to services.lavis.user.";
      example = "users";
    };

    home = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Home directory used to derive Lavis XDG paths. Defaults to the configured user's home.";
      example = "/home/melvi";
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
          assertion = cfg.user != null;
          message = "services.lavis.user must be set to an existing Unix user.";
        }
        {
          assertion = cfg.home != null || declaredUserHome != null;
          message = "services.lavis.home must be set when services.lavis.user has no declared home.";
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
      };

      preStart = "${setupScript}";

      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/lavis run";
        User = cfg.user;
        Group = effectiveGroup;
        WorkingDirectory = effectiveHome;
        Restart = "on-failure";
        RestartSec = "5s";
        PermissionsStartOnly = true;
        EnvironmentFile = optional (cfg.credentialsEnvironmentFile != null) cfg.credentialsEnvironmentFile;
      };
    };
  };
}
