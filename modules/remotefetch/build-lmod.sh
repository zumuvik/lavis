#!/usr/bin/env bash
set -euo pipefail
export TZ=UTC
cd "$(dirname "$0")"
rm -rf build
mkdir -p build dist
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o build/remotefetch .
chmod 700 build/remotefetch
cp module.json build/module.json
chmod 600 build/module.json
touch -t 198001010000 build/remotefetch build/module.json
rm -f dist/remotefetch.lmod
(
  cd build
  zip -q -0 -X ../dist/remotefetch.lmod module.json remotefetch
)
printf 'built %s\n' "$(realpath dist/remotefetch.lmod)"
