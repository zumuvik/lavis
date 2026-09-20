{
  description = "Lavis Telegram userbot foundation";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs supportedSystems f;
      perSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          goArch =
            if pkgs.stdenv.hostPlatform.isx86_64 then
              "amd64"
            else if pkgs.stdenv.hostPlatform.isAarch64 then
              "arm64"
            else
              throw "lavis: unsupported system ${system}";
          extensionLib = import ./nix/lib/extensions.nix { inherit pkgs; };
          rustSourceRoot = toString self.outPath;
          rustSource = pkgs.lib.cleanSourceWith {
            name = "lavis-rust-source";
            src = self;
            filter =
              path: type:
              let
                relative = pkgs.lib.removePrefix "/" (
                  pkgs.lib.removePrefix rustSourceRoot (toString path)
                );
                topLevel = builtins.head (pkgs.lib.splitString "/" relative);
                rustInput =
                  relative == ""
                  || builtins.elem relative [
                    "Cargo.toml"
                    "Cargo.lock"
                    "build.rs"
                  ]
                  || builtins.elem topLevel [
                    ".cargo"
                    "src"
                    "tests"
                    "examples"
                    "benches"
                    "tools"
                    "protocol"
                    "assets"
                  ];
              in
              pkgs.lib.cleanSourceFilter path type && rustInput;
          };
          corePackage = pkgs.rustPlatform.buildRustPackage {
            pname = "lavis";
            version = "1.2.0";
            src = rustSource;
            cargoLock.lockFile = ./Cargo.lock;
            # rustc spawns helper threads in addition to Cargo's workers. Leaving
            # all logical CPUs available here can exhaust a host/cgroup pid budget
            # during a larger NixOS rebuild and turn that pressure into a rustc ICE.
            preBuild = ''
              if [ "$NIX_BUILD_CORES" -gt 4 ]; then
                export NIX_BUILD_CORES=4
              fi
            '';
            preCheck = ''
              if [ "$NIX_BUILD_CORES" -gt 4 ]; then
                export NIX_BUILD_CORES=4
              fi
              export XDG_STATE_HOME="$TMPDIR/lavis-test-state"
              install -d -m 700 "$XDG_STATE_HOME"
            '';
            nativeBuildInputs = [
              pkgs.makeWrapper
              pkgs.python3 # JSON-line external-process fixtures (test-only)
              pkgs.util-linux # `flock` session-lock test helper
            ];
            postFixup = ''
              wrapProgram $out/bin/lavis \
                --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.fastfetch ]} \
                --set-default LAVIS_HOST nix-package
            '';
            meta = {
              description = "Personal Telegram userbot written in Rust";
              homepage = "https://github.com/zumuvik/lavis";
              license = pkgs.lib.licenses.gpl3Only;
              mainProgram = "lavis";
              platforms = pkgs.lib.platforms.linux;
            };
          };
          # Keep the Git revision in a cheap outer wrapper instead of the Rust
          # derivation. Documentation/Nix/module-only commits can then reuse the
          # already compiled core package while `,info` still reports the real HEAD.
          package = pkgs.symlinkJoin {
            name = "lavis-${corePackage.version}";
            paths = [ corePackage ];
            nativeBuildInputs = [ pkgs.makeWrapper ];
            postBuild = ''
              wrapProgram "$out/bin/lavis" \
                --set LAVIS_GIT_REV ${pkgs.lib.escapeShellArg (self.rev or "dirty")}
            '';
            meta = corePackage.meta;
          };
          # Every bundled Go module builds the same way: one static binary plus
          # the manifest, laid out exactly as the .lmod archive root expects.
          goExtension =
            {
              id,
              version,
              src,
              description,
              binary ? id,
              path ? binary,
            }:
            pkgs.stdenvNoCC.mkDerivation {
              pname = "lavis-extension-${id}";
              inherit version src;
              nativeBuildInputs = [ pkgs.go ];
              buildPhase = ''
                runHook preBuild
                export GOCACHE="$TMPDIR/go-cache"
                export CGO_ENABLED=0
                export GOOS=linux
                export GOARCH=${goArch}
                go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o ${binary} .
                runHook postBuild
              '';
              installPhase = ''
                runHook preInstall
                install -Dm700 ${binary} "$out/${path}"
                install -Dm600 module.json "$out/module.json"
                runHook postInstall
              '';
              meta = {
                inherit description;
                license = pkgs.lib.licenses.gpl3Only;
                platforms = pkgs.lib.platforms.linux;
              };
            };
          # Packs an already-built extension into the stored-only ZIP the host
          # inspector accepts. Modes and timestamps are pinned so the archive is
          # byte-reproducible, matching modules/<id>/build-lmod.sh.
          lmodArchive =
            {
              id,
              extension,
              path,
            }:
            pkgs.runCommand "lavis-${id}.lmod"
              {
                nativeBuildInputs = [ pkgs.zip ];
              }
              ''
                mkdir -p build
                install -Dm700 ${extension}/${path} build/${path}
                install -Dm600 ${extension}/module.json build/module.json
                TZ=UTC touch -t 198001010000 build/${path} build/module.json
                cd build
                zip -q -0 -X "$out" module.json ${path}
              '';
          gafExtension = goExtension {
            id = "gaf";
            version = "0.2.0";
            src = ./modules/gaf;
            description = "GAF external module for Lavis";
          };
          zaiExtension = goExtension {
            id = "zai";
            version = "0.3.0";
            src = ./modules/zai;
            description = "Z.AI quota external module for Lavis";
          };
          remotefetchExtension = goExtension {
            id = "remotefetch";
            version = "0.1.0";
            src = ./modules/remotefetch;
            description = "Remote fastfetch external module for Lavis";
          };
          # cleaner is the only bundled module with a third-party dependency
          # (gotd/td), so its modules are fetched rather than built from a bare
          # source tree. The host expects bin/cleaner plus a root manifest, not
          # Go's default $out/bin layout.
          cleanerExtension = pkgs.buildGoModule {
            pname = "lavis-extension-cleaner";
            version = "0.3.1";
            src = ./modules/cleaner;
            vendorHash = "sha256-vnbaia5400XRNMsCjwE0pJM0TPF9EsiY8ek3/xnd+j0=";
            env.CGO_ENABLED = 0;
            # buildGoModule already passes -buildid= itself.
            ldflags = [
              "-s"
              "-w"
            ];
            installPhase = ''
              runHook preInstall
              install -Dm700 "$GOPATH/bin/cleaner" "$out/bin/cleaner"
              install -Dm600 module.json "$out/module.json"
              runHook postInstall
            '';
            meta = {
              description = "Cleaner external module for Lavis";
              license = pkgs.lib.licenses.gpl3Only;
              platforms = pkgs.lib.platforms.linux;
            };
          };
          fetchdAgent = pkgs.stdenvNoCC.mkDerivation {
            pname = "lavis-fetchd";
            version = "0.1.0";
            src = ./modules/remotefetch;
            nativeBuildInputs = [ pkgs.go ];
            buildPhase = ''
              runHook preBuild
              export GOCACHE="$TMPDIR/go-cache"
              export CGO_ENABLED=0
              export GOOS=linux
              export GOARCH=${goArch}
              go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o lavis-fetchd ./agent
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              install -Dm755 lavis-fetchd "$out/bin/lavis-fetchd"
              runHook postInstall
            '';
            meta = {
              description = "Fastfetch agent queried by the Lavis remotefetch module";
              license = pkgs.lib.licenses.gpl3Only;
              platforms = pkgs.lib.platforms.linux;
              mainProgram = "lavis-fetchd";
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
                if [ "$1" = "validate-prefix" ]; then
                  test "$#" -eq 2
                  # Mirror the Rust validator's visible-non-alphabetic rule so the
                  # eval test proves the preStart wiring rejects invalid prefixes.
                  case "$2" in
                    "⚙️"|"a"|"ab"|" "|"")
                      exit 1
                      ;;
                  esac
                  exit 0
                fi
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

              mkdir -p '/build/lavis-test home/.local/state/lavis'
              printf '%s\n' '{"version":2,"prefix":"!","locale":"en","onboarding":"companion"}' \
                > '/build/lavis-test home/.local/state/lavis/settings.json'
              chmod 600 '/build/lavis-test home/.local/state/lavis/settings.json'
              "$preStartScript"
              "$preStartScript"
              ${pkgs.python3}/bin/python3 - <<'PY'
import json
with open('/build/lavis-test home/.local/state/lavis/settings.json', encoding='utf-8') as handle:
    state = json.load(handle)
if state != {"version": 2, "prefix": ".", "locale": "en", "onboarding": "companion"}:
    raise SystemExit(state)
PY
              printf '%s\n' '{"enabled":true,"next_id":2,"triggers":[{"id":1,"word":"nix","reactions":[{"type":"emoji","emoji":"👍"}],"enabled":true}],"active":{}}' \
                > '/build/lavis-test home/.local/share/lavis/modules/fixture/state.json'
              "$preStartScript"
              test -f '/build/lavis-test home/.local/state/lavis/modules/fixture/state.json'
              grep -q '"word":"nix"' '/build/lavis-test home/.local/state/lavis/modules/fixture/state.json'
              test ! -f '/build/lavis-test home/.local/share/lavis/modules/fixture/state.json'
              # A later activation must not clobber already-migrated module state:
              # the persistent state directory wins over a newly bundled state.json.
              printf '%s\n' '{"enabled":true,"next_id":3,"triggers":[{"id":2,"word":"runtime","reactions":[{"type":"emoji","emoji":"✅"}],"enabled":true}],"active":{}}' \
                > '/build/lavis-test home/.local/share/lavis/modules/fixture/state.json'
              "$preStartScript"
              test -f '/build/lavis-test home/.local/state/lavis/modules/fixture/state.json'
              grep -q '"word":"nix"' '/build/lavis-test home/.local/state/lavis/modules/fixture/state.json'
              test ! -f '/build/lavis-test home/.local/share/lavis/modules/fixture/state.json'

              # The preStart delegates prefix validation to `lavis validate-prefix`.
              # An existing settings file carrying an invalid prefix (written before
              # prefix validation existed) must abort activation without being
              # rewritten, so the bad state is never silently canonicalized.
              printf '%s\n' '{"version":2,"prefix":"⚙️","locale":"en","onboarding":"companion"}' \
                > '/build/lavis-test home/.local/state/lavis/settings.json'
              chmod 600 '/build/lavis-test home/.local/state/lavis/settings.json'
              if "$preStartScript" 2>prefix-invalid.log; then
                echo "preStart accepted an existing settings file with an invalid prefix" >&2
                exit 1
              fi
              grep -q 'existing Lavis settings have an unsupported schema' prefix-invalid.log
              grep -q '"prefix":"⚙️"' '/build/lavis-test home/.local/state/lavis/settings.json'
              printf '%s\n' '{"version":2,"prefix":"!","locale":"en","onboarding":"companion"}' \
                > '/build/lavis-test home/.local/state/lavis/settings.json'
              chmod 600 '/build/lavis-test home/.local/state/lavis/settings.json'
              "$preStartScript"

              touch "$out"
            '';
          fetchdEvalCheck =
            let
              evaluated = nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  self.nixosModules.fetchd
                  (
                    { ... }:
                    {
                      users.users.laptop-owner = {
                        isNormalUser = true;
                        home = "/home/laptop-owner";
                      };
                      services.lavis-fetchd = {
                        enable = true;
                        user = "laptop-owner";
                        listenAddress = "100.120.95.96:8471";
                        tokenFile = "/run/secrets/lavis-fetchd-token";
                        openFirewall = true;
                      };
                    }
                  )
                ];
              };
            in
            pkgs.runCommand "lavis-fetchd-module-eval"
              {
                unit = evaluated.config.systemd.user.units."lavis-fetchd.service".unit;
                ports = builtins.toJSON evaluated.config.networking.firewall.interfaces.tailscale0.allowedTCPPorts;
                linger = nixpkgs.lib.boolToString evaluated.config.users.users.laptop-owner.linger;
              }
              ''
                grep -q 'ConditionUser=laptop-owner' "$unit/lavis-fetchd.service"
                grep -q -- '--listen 100.120.95.96:8471' "$unit/lavis-fetchd.service"
                grep -q -- '--token-file /run/secrets/lavis-fetchd-token' "$unit/lavis-fetchd.service"
                # File-reading fastfetch options stay off unless asked for.
                ! grep -q -- '--allow-file-args' "$unit/lavis-fetchd.service"
                test "$ports" = '[8471]'
                test "$linger" = true
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
          prefixValidatorCheck = pkgs.runCommand "lavis-prefix-validator-check" { } ''
            set -euo pipefail
            # The NixOS module delegates prefix validation to this exact binary, so
            # the fixtures below must match the Rust validator's behavior. These
            # are the known drift cases: variation selectors (U+FE0F, "⚙️"),
            # zero-width/invisible characters, and ordinary visible emoji.
            ${package}/bin/lavis validate-prefix "🦀"
            ${package}/bin/lavis validate-prefix "!"
            ${package}/bin/lavis validate-prefix ","
            for prefix in \
              "⚙️" \
              "$(printf '\u200b')" \
              "$(printf '\ufeff')" \
              "$(printf '\u200d')" \
              "$(printf '\u2060')" \
              "$(printf '\ufe0f')" \
              "a" \
              "ab" \
              " " \
              "$(printf '\u00ad')"; do
              if ${package}/bin/lavis validate-prefix "$prefix"; then
                echo "invalid prefix accepted by the Rust validator: $prefix" >&2
                exit 1
              fi
            done
            touch "$out"
          '';
        in
        {
          lib = extensionLib;
          packages = {
            default = package;
            lavis-extension-cleaner = cleanerExtension;
            lavis-extension-gaf = gafExtension;
            lavis-extension-zai = zaiExtension;
            lavis-extension-remotefetch = remotefetchExtension;
            lavis-fetchd = fetchdAgent;
            # Installable .lmod archives, replacing the binaries that used to be
            # committed under modules/*/dist/.
            lavis-lmod-cleaner = lmodArchive {
              id = "cleaner";
              extension = cleanerExtension;
              path = "bin/cleaner";
            };
            lavis-lmod-gaf = lmodArchive {
              id = "gaf";
              extension = gafExtension;
              path = "gaf";
            };
            lavis-lmod-zai = lmodArchive {
              id = "zai";
              extension = zaiExtension;
              path = "zai";
            };
            lavis-lmod-remotefetch = lmodArchive {
              id = "remotefetch";
              extension = remotefetchExtension;
              path = "remotefetch";
            };
          };
          # Separate wrapper for `nix run` so it reports "Host: nix run" without
          # affecting the base package used by NixOS module or systemPackages.
          apps = {
            default =
              let
                nixRunWrapper = pkgs.writeShellScriptBin "lavis" ''
                  export LAVIS_HOST="nix-run"
                  exec ${package}/bin/lavis "$@"
                '';
              in
              {
                type = "app";
                program = "${nixRunWrapper}/bin/lavis";
              };
          };
          devShells = {
            default = pkgs.mkShell {
              packages = with pkgs; [
                cargo
                clippy
                rustc
                rustfmt
                fastfetch
                python3 # JSON-line external-process fixtures
                util-linux # `flock` session-lock test helper
                go
                zip
              ];
            };
          };
          checks = {
            default = package;
            merge-enabled-extensions = mergeEnabledCheck;
            nixos-module = moduleEvalCheck;
            nixos-fetchd-module = fetchdEvalCheck;
            prefix-validator = prefixValidatorCheck;
          };
        };
    in
    {
      nixosModules.default = import ./nix/modules/lavis.nix { inherit self; };
      nixosModules.fetchd = import ./nix/modules/lavis-fetchd.nix { inherit self; };
      lib = forAllSystems (system: (perSystem system).lib);
      packages = forAllSystems (system: (perSystem system).packages);
      apps = forAllSystems (system: (perSystem system).apps);
      devShells = forAllSystems (system: (perSystem system).devShells);
      checks = forAllSystems (system: (perSystem system).checks);
    };
}
