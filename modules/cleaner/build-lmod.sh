#!/usr/bin/env bash
set -euo pipefail
export TZ=UTC
cd "$(dirname "$0")"
rm -rf build
mkdir -p build/bin dist
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o build/bin/cleaner .
chmod 700 build/bin/cleaner
cp module.json build/module.json
chmod 600 build/module.json
touch -t 198001010000 build/bin/cleaner build/module.json
rm -f dist/cleaner.lmod
(
  cd build
  zip -q -0 -X ../dist/cleaner.lmod module.json bin/cleaner
)
printf 'built %s\n' "$(realpath dist/cleaner.lmod)"
