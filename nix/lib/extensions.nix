{ pkgs }:

{
  buildLavisExtensionFromLmod =
    {
      id,
      src,
    }:
    pkgs.stdenvNoCC.mkDerivation {
      pname = "lavis-extension-${id}";
      version = "0.1.0";
      inherit src;

      nativeBuildInputs = [ pkgs.unzip ];

      unpackPhase = ''
        runHook preUnpack
        mkdir source
        unzip -qq "$src" -d source
        runHook postUnpack
      '';

      installPhase = ''
        runHook preInstall
        if [ ! -f source/module.json ]; then
          echo "lavis extension ${id}: module.json is missing at archive root" >&2
          exit 1
        fi
        mkdir -p "$out"
        cp -R --no-preserve=ownership source/. "$out/"
        runHook postInstall
      '';
    };
}
