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
        nativeBuildInputs = [ pkgs.makeWrapper ];
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
          evaluated = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              (
                { ... }:
                {
                  users.users.lavis-test = {
                    isNormalUser = true;
                    group = "lavis-test";
                    home = "/var/lib/lavis-test";
                  };
                  users.groups.lavis-test = { };
                  services.lavis = {
                    enable = true;
                    user = "lavis-test";
                    credentialsEnvironmentFile = "/run/secrets/lavis.env";
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
        in
        pkgs.runCommand "lavis-nixos-module-eval" {
          unit = evaluated.config.systemd.units."lavis.service".unit;
          preStartScript = evaluated.config.systemd.services.lavis.preStart;
        } ''
          grep -q 'User=lavis-test' "$unit/lavis.service"
          grep -q 'EnvironmentFile=/run/secrets/lavis.env' "$unit/lavis.service"
          grep -q 'XDG_STATE_HOME=/var/lib/lavis-test/.local/state' "$unit/lavis.service"
          grep -q 'mktemp -d -p /var/lib/lavis-test/.local/share/lavis/module-staging' "$preStartScript"
          ! grep -q 'chown -R' "$preStartScript"
          ! grep -q 'PermissionsStartOnly=true' "$unit/lavis.service"
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
