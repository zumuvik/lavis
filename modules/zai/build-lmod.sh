#!/usr/bin/env bash
set -euo pipefail
export TZ=UTC
cd "$(dirname "$0")"
rm -rf build
mkdir -p build dist
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o build/zai .
chmod 700 build/zai
cp module.json build/module.json
chmod 600 build/module.json
touch -t 198001010000 build/zai build/module.json
rm -f dist/zai.lmod
(
  cd build
  zip -q -0 -X ../dist/zai.lmod module.json zai
)
printf 'built %s\n' "$(realpath dist/zai.lmod)"
