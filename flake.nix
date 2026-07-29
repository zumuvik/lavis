{
  description = "Lavis Telegram userbot foundation";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
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
    in
    {
      packages.${system}.default = package;
      apps.${system}.default = {
        type = "app";
        program = "${package}/bin/lavis";
      };
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
      checks.${system}.default = package;
    };
}
