#!/usr/bin/env bash
# Write reader.exe from scripts/reader-exe.S (or another program written
# the same way, given as SOURCE, such as spin-exe.S): GNU as assembles the
# whole PE file as one flat section, and objcopy takes its bytes out. See
# the source for why no linker is needed.
set -euo pipefail
out=${1:?usage: reader-exe.sh OUT [SOURCE]}
here=$(cd "$(dirname "$0")" && pwd)
source=${2:-reader-exe.S}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
as --64 -o "$tmp/reader.o" "$here/$source"
objcopy -O binary -j .text "$tmp/reader.o" "$out"
