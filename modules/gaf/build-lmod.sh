#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
rm -rf build
mkdir -p build dist
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o build/gaf .
chmod 700 build/gaf
cp module.json build/module.json
chmod 600 build/module.json
TZ=UTC touch -t 198001010000 build/gaf build/module.json
rm -f dist/gaf.lmod
(
  cd build
  zip -q -0 -X ../dist/gaf.lmod module.json gaf
)
printf 'built %s\n' "$(realpath dist/gaf.lmod)"
