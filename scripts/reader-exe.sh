#!/usr/bin/env bash
# Write reader.exe from scripts/reader-exe.S: GNU as assembles the whole
# PE file as one flat section, and objcopy takes its bytes out. See the
# source for why no linker is needed.
set -euo pipefail
out=${1:?usage: reader-exe.sh OUT}
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
as --64 -o "$tmp/reader.o" "$here/reader-exe.S"
objcopy -O binary -j .text "$tmp/reader.o" "$out"
