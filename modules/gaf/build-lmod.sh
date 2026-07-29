#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p build dist
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -ldflags='-s -w' -o build/gaf .
chmod 700 build/gaf
cp module.json build/module.json
chmod 600 build/module.json
(
  cd build
  zip -q -0 -X ../dist/gaf.lmod module.json gaf
)
printf 'built %s\n' "$(realpath dist/gaf.lmod)"
