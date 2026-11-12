#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
rm -rf build
mkdir -p build dist
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o build/typing .
chmod 700 build/typing
cp module.json build/module.json
chmod 600 build/module.json
touch -t 198001010000 build/typing build/module.json
rm -f dist/typing.lmod
(cd build && zip -q -0 -X ../dist/typing.lmod module.json typing)
printf 'built %s\n' "$(realpath dist/typing.lmod)"
