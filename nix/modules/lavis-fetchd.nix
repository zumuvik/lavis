{ self }:

{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    mkDefault
    mkEnableOption
    mkIf
    mkOption
    types
    ;

  cfg = config.services.lavis-fetchd;
  system = pkgs.stdenv.hostPlatform.system;
in
{
  options.services.lavis-fetchd = {
    enable = mkEnableOption "the Lavis fastfetch agent for this machine";

    package = mkOption {
      type = types.package;
      default = self.packages.${system}.lavis-fetchd;
      defaultText = lib.literalExpression "inputs.lavis.packages.\${pkgs.system}.lavis-fetchd";
      description = "The lavis-fetchd package to run.";
    };

    user = mkOption {
      type = types.str;
      example = "alice";
      description = ''
        Desktop user whose session the agent reports. The agent runs as a
        systemd user service for exactly this account, because fastfetch reads
        the session environment to detect the desktop, window manager, terminal
        and displays. A system service running as root would report the
        hardware correctly and leave those fields empty.
      '';
    };

    listenAddress = mkOption {
      type = types.str;
      example = "100.120.95.96:8471";
      description = ''
        Address to listen on. Use the machine's address on a private network
        such as Tailscale or WireGuard; the agent has no transport encryption
        of its own and must not be exposed to an untrusted network.
      '';
    };

    tokenFile = mkOption {
      type = types.path;
      example = "/run/secrets/lavis-fetchd-token";
      description = ''
        File holding the shared bearer token, readable by
        <option>services.lavis-fetchd.user</option>. The same token goes into
        the userbot module's config.json. The agent refuses to start if the
        file is world-readable or holds fewer than 16 bytes. Do not put the
        token in the Nix store: every store path is world-readable.
      '';
    };

    timeout = mkOption {
      type = types.str;
      default = "2500ms";
      description = ''
        Maximum fastfetch run time per request. It must stay below the
        userbot's own request timeout, which in turn stays below the five
        second Module API v6 lifecycle deadline.
      '';
    };

    allowFileArgs = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Permit the fastfetch options that read a caller-chosen path
        (<literal>--file</literal>, <literal>--raw</literal>,
        <literal>--config</literal>, <literal>--logo-type</literal> and
        friends). They are refused by default: the agent is meant to publish
        fastfetch output, not this machine's filesystem, and whoever holds the
        token would otherwise be able to read any file the user can.
      '';
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Open the listening port on the Tailscale interface. Leave this off when
        the tailnet interface is already trusted by
        <option>networking.firewall.trustedInterfaces</option>.
      '';
    };

    interface = mkOption {
      type = types.str;
      default = "tailscale0";
      description = "Interface the firewall rule applies to when openFirewall is set.";
    };

    linger = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Keep the user's systemd instance running while they are logged out, so
        the agent answers a closed-lid laptop that is still awake. Without it
        the user service only exists during an active login session.
      '';
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.listenAddress != "";
        message = "services.lavis-fetchd.listenAddress must be set to a private network address.";
      }
    ];

    warnings =
      lib.optional (lib.hasPrefix "0.0.0.0:" cfg.listenAddress || lib.hasPrefix ":" cfg.listenAddress)
        "services.lavis-fetchd.listenAddress binds every interface; the agent has no TLS and should listen on a private address only.";

    users.users.${cfg.user}.linger = mkDefault cfg.linger;

    # Scoped with mkIf so a disabled openFirewall does not materialize an
    # interface rule set that the host had not defined at all.
    networking.firewall.interfaces = mkIf cfg.openFirewall {
      ${cfg.interface}.allowedTCPPorts = [
        (lib.toInt (lib.last (lib.splitString ":" cfg.listenAddress)))
      ];
    };

    systemd.user.services.lavis-fetchd = {
      description = "Lavis fastfetch agent";
      wantedBy = [ "default.target" ];
      after = [ "graphical-session.target" ];
      # The unit is defined for every user instance; this pins it to the one
      # desktop account whose session fastfetch is supposed to describe.
      unitConfig.ConditionUser = cfg.user;

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.escapeShellArgs (
          [
            "${cfg.package}/bin/lavis-fetchd"
            "--listen"
            cfg.listenAddress
            "--token-file"
            "${cfg.tokenFile}"
            "--timeout"
            cfg.timeout
          ]
          ++ lib.optional cfg.allowFileArgs "--allow-file-args"
        );
        Restart = "on-failure";
        RestartSec = "5s";

        # fastfetch reads hardware, the session bus and the filesystem, so the
        # sandbox only removes what it demonstrably never needs.
        NoNewPrivileges = true;
        ProtectClock = true;
        ProtectKernelLogs = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@resources"
        ];
      };

      path = [
        pkgs.fastfetch
      ];
    };

    environment.systemPackages = [ cfg.package ];
  };
}
