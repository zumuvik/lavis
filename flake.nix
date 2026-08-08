{
  description = "Lavis Telegram userbot foundation";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      extensionLib = import ./nix/lib/extensions.nix { inherit pkgs; };
      package = pkgs.rustPlatform.buildRustPackage {
        pname = "lavis";
        version = "0.1.0";
        src = pkgs.lib.cleanSourceWith {
          src = self;
          filter =
            path: type:
            pkgs.lib.cleanSourceFilter path type
            && builtins.baseNameOf path != "result"
            && builtins.baseNameOf path != "target";
        };
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [
          pkgs.makeWrapper
          pkgs.python3 # JSON-line external-process fixtures (test-only)
        ];
        postFixup = ''
          wrapProgram $out/bin/lavis \
            --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.fastfetch ]}
        '';
        meta = {
          description = "Personal Telegram userbot written in Rust";
          homepage = "https://github.com/zumuvik/lavis";
          license = pkgs.lib.licenses.gpl3Only;
          mainProgram = "lavis";
          platforms = pkgs.lib.platforms.linux;
        };
      };
      gafExtension = pkgs.stdenvNoCC.mkDerivation {
        pname = "lavis-extension-gaf";
        version = "0.1.0";
        src = ./modules/gaf;
        nativeBuildInputs = [ pkgs.go ];
        buildPhase = ''
          runHook preBuild
          export GOCACHE="$TMPDIR/go-cache"
          export CGO_ENABLED=0
          export GOOS=linux
          export GOARCH=amd64
          go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o gaf .
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          install -Dm700 gaf "$out/gaf"
          install -Dm600 module.json "$out/module.json"
          runHook postInstall
        '';
        meta = {
          description = "GAF external module for Lavis";
          license = pkgs.lib.licenses.gpl3Only;
          platforms = pkgs.lib.platforms.linux;
        };
      };
      moduleEvalCheck =
        let
          fixtureExtension = pkgs.runCommand "lavis-extension-fixture" { } ''
            mkdir -p "$out/bin"
            cp ${
              pkgs.writeText "module.json" ''
                {
                  "schema_version": 2,
                  "id": "fixture",
                  "name": "Fixture",
                  "version": "0.1.0",
                  "author": "Lavis",
                  "entrypoint": "bin/fixture",
                  "capabilities": [],
                  "commands": [
                    {
                      "name": "fixture",
                      "summary_ru": "Fixture",
                      "description_ru": "Fixture command.",
                      "usage": "[text]",
                      "examples": []
                    }
                  ]
                }
              ''
            } "$out/module.json"
            cp ${pkgs.writeShellScript "fixture" "cat"} "$out/bin/fixture"
            chmod 600 "$out/module.json"
            chmod 700 "$out/bin/fixture"
          '';
          authFixture = pkgs.writeShellScriptBin "lavis" ''
            test "$#" -eq 1
            test "$1" = auth
            printf '%s\n' "$HOME" > "$HOME/auth-home"
            printf '%s:%s\n' "$LAVIS_API_ID" "$LAVIS_API_HASH" > "$HOME/auth-credentials"
          '';
          fakeRunuser = pkgs.writeShellScript "lavis-test-runuser" ''
            set -euo pipefail
            while [ "$#" -gt 0 ]; do
              case "$1" in
                --user|--group)
                  shift 2
                  ;;
                --)
                  shift
                  exec "$@"
                  ;;
                *)
                  echo "unexpected runuser argument: $1" >&2
                  exit 1
                  ;;
              esac
            done
            echo "runuser command missing" >&2
            exit 1
          '';
          evaluated = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              (
                { ... }:
                {
                  users.users.lavis-test = {
                    isNormalUser = true;
                    home = "/build/lavis-test home";
                  };
                  services.lavis = {
                    enable = true;
                    package = authFixture;
                    user = "lavis-test";
                    credentialsEnvironmentFile = "/build/lavis-test home/secrets/lavis.env";
                    settings.prefix = ".";
                    fastfetchProfile = {
                      version = 1;
                      logo = "NixOS";
                      structure = [ "title" "os" "kernel" ];
                    };
                    extensions = [
                      {
                        id = "fixture";
                        package = fixtureExtension;
                      }
                    ];
                  };
                }
              )
            ];
          };
          defaultEvaluated = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              (
                { ... }:
                {
                  services.lavis = {
                    enable = true;
                    credentialsEnvironmentFile = "/run/secrets/lavis.env";
                  };
                }
              )
            ];
          };
          customAuthPackage = builtins.head (
            builtins.filter
              (package: (package.name or "") == "lavis-auth")
              evaluated.config.environment.systemPackages
          );
        in
        pkgs.runCommand "lavis-nixos-module-eval" {
          unit = evaluated.config.systemd.units."lavis.service".unit;
          preStartScript = evaluated.config.systemd.services.lavis.preStart;
          authScript = "${customAuthPackage}/bin/lavis-auth";
          defaultUnit = defaultEvaluated.config.systemd.units."lavis.service".unit;
          existingTmpfiles = pkgs.writeText "lavis-existing-user-tmpfiles" (
            nixpkgs.lib.concatStringsSep "\n" evaluated.config.systemd.tmpfiles.rules
          );
          defaultTmpfiles = pkgs.writeText "lavis-default-tmpfiles" (
            nixpkgs.lib.concatStringsSep "\n" defaultEvaluated.config.systemd.tmpfiles.rules
          );
        } ''
          grep -q 'User=lavis-test' "$unit/lavis.service"
          grep -q 'Group=users' "$unit/lavis.service"
          ! grep -q 'Group=lavis-test' "$unit/lavis.service"
          grep -q 'EnvironmentFile=/build/lavis-test home/secrets/lavis.env' "$unit/lavis.service"
          grep -q 'XDG_STATE_HOME=/build/lavis-test home/.local/state' "$unit/lavis.service"
          grep -q "mktemp -d -p '/build/lavis-test home/.local/share/lavis/module-staging'" "$preStartScript"
          grep -q 'runuser' "$authScript"
          grep -q 'XDG_STATE_HOME=/build/lavis-test home/.local/state' "$authScript"
          ! grep -q "XDG_STATE_HOME='/build/lavis-test home/.local/state'" "$authScript"
          grep -q '/build/lavis-test home/secrets/lavis.env' "$authScript"
          grep -q 'expected literal LAVIS_API_ID' "$authScript"
          grep -q '32 hexadecimal characters' "$authScript"
          ! grep -q 'install -d -m 700 -o lavis-test' "$authScript"
          ! grep -q 'set -a' "$authScript"
          ! grep -q '\. /run/secrets/lavis.env' "$authScript"
          mkdir -p '/build/lavis-test home/secrets'
          printf '%s\n' 'LAVIS_API_ID=not-digits' 'LAVIS_API_HASH=0123456789abcdef0123456789abcdef' > '/build/lavis-test home/secrets/lavis.env'
          if ${pkgs.fakeroot}/bin/fakeroot "$authScript" 2>auth-invalid.log; then
            echo "lavis-auth accepted invalid credentialsEnvironmentFile" >&2
            exit 1
          fi
          grep -q 'LAVIS_API_ID must be decimal digits' auth-invalid.log
          ! grep -q 'runuser' auth-invalid.log
          ! grep -q 'lavis-test' auth-invalid.log
          printf '%s\n' 'LAVIS_API_ID=123456789' 'LAVIS_API_HASH=0123456789abcdef0123456789abcdef' > '/build/lavis-test home/secrets/lavis.env'
          cp "$authScript" auth-success
          substituteInPlace auth-success --replace-fail '${pkgs.util-linux}/bin/runuser' '${fakeRunuser}'
          ${pkgs.fakeroot}/bin/fakeroot ./auth-success
          test -f '/build/lavis-test home/auth-home'
          grep -qxF '/build/lavis-test home' '/build/lavis-test home/auth-home'
          grep -qxF '123456789:0123456789abcdef0123456789abcdef' '/build/lavis-test home/auth-credentials'
          test -d '/build/lavis-test home/.config/lavis'
          test -d '/build/lavis-test home/.local/state/lavis'
          test -d '/build/lavis-test home/.local/share/lavis'
          ! grep -q 'd /build/lavis-test home 0700' "$existingTmpfiles"
          grep -q 'User=lavis' "$defaultUnit/lavis.service"
          grep -q 'Group=lavis' "$defaultUnit/lavis.service"
          grep -q 'WorkingDirectory=/var/lib/lavis' "$defaultUnit/lavis.service"
          grep -q 'd /var/lib/lavis 0700 lavis lavis - -' "$defaultTmpfiles"
          ! grep -q 'chown -R' "$preStartScript"
          ! grep -q 'PermissionsStartOnly=true' "$unit/lavis.service"

          "$preStartScript"
          printf '%s\n' '{"enabled":true,"next_id":2,"triggers":[{"id":1,"word":"nix","reactions":[{"type":"emoji","emoji":"👍"}],"enabled":true}],"active":{}}' \
            > '/build/lavis-test home/.local/share/lavis/modules/fixture/state.json'
          "$preStartScript"
          test -f '/build/lavis-test home/.local/state/lavis/modules/fixture/state.json'
          grep -q '"word":"nix"' '/build/lavis-test home/.local/state/lavis/modules/fixture/state.json'
          test ! -f '/build/lavis-test home/.local/share/lavis/modules/fixture/state.json'
          printf '%s\n' '{"enabled":true,"next_id":3,"triggers":[{"id":2,"word":"runtime","reactions":[{"type":"emoji","emoji":"✅"}],"enabled":true}],"active":{}}' \
            > '/build/lavis-test home/.local/state/lavis/modules/fixture/state.json'
          "$preStartScript"
          test -f '/build/lavis-test home/.local/state/lavis/modules/fixture/state.json'
          grep -q '"word":"runtime"' '/build/lavis-test home/.local/state/lavis/modules/fixture/state.json'
          test ! -f '/build/lavis-test home/.local/share/lavis/modules/fixture/state.json'

          touch "$out"
        '';
      mergeEnabledCheck = pkgs.runCommand "lavis-merge-enabled-extensions-check" { } ''
        work="$TMPDIR/lavis-state"
        mkdir -p "$work"
        printf '%s\n' '{"version":1,"enabled":["manual","old-decl","enabled-decl"]}' > "$work/external-modules.json"
        printf '%s\n' '["old-decl","enabled-decl"]' > "$work/declarative-modules.json"
        printf '%s\n' '["enabled-decl"]' > "$work/current-declarative.json"
        printf '%s\n' '[]' > "$work/current-enabled.json"

        ${pkgs.python3}/bin/python3 ${./nix/modules/merge-enabled-extensions.py} \
          "$work/external-modules.json" \
          "$work/declarative-modules.json" \
          "$work/current-declarative.json" \
          "$work/current-enabled.json"

        ${pkgs.python3}/bin/python3 - "$work" <<'PY'
import json
import os
import sys

work = sys.argv[1]
with open(os.path.join(work, "external-modules.json"), "r", encoding="utf-8") as handle:
    state = json.load(handle)
if state != {"version": 1, "enabled": ["manual"]}:
    raise SystemExit(state)
with open(os.path.join(work, "declarative-modules.json"), "r", encoding="utf-8") as handle:
    declarative = json.load(handle)
if declarative != ["enabled-decl"]:
    raise SystemExit(declarative)
if os.path.exists(os.path.join(work, ".external-modules.json.nixos.tmp")):
    raise SystemExit("predictable temporary file exists")
PY
        touch "$out"
      '';
    in
    {
      lib.${system} = extensionLib;
      packages.${system} = {
        default = package;
        lavis-extension-gaf = gafExtension;
      };
      apps.${system}.default = {
        type = "app";
        program = "${package}/bin/lavis";
      };
      nixosModules.default = import ./nix/modules/lavis.nix { inherit self; };
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          clippy
          rustc
          rustfmt
          fastfetch
          python3 # JSON-line external-process fixtures
          go
          zip
        ];
      };
      checks.${system} = {
        default = package;
        merge-enabled-extensions = mergeEnabledCheck;
        nixos-module = moduleEvalCheck;
      };
    };
}
