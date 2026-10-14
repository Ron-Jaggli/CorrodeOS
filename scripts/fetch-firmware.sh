#!/usr/bin/env bash
#
# Put the NVIDIA GSP firmware for an RTX 3060 Ti (GA104) in firmware/, where
# the Makefile picks it up and puts it on the ISO as modules.
#
# The firmware is NVIDIA's, redistributed in linux-firmware under NVIDIA's
# own licence (see LICENCE.nvidia there). It is never committed here.
#
#   scripts/fetch-firmware.sh                 copy from /lib/firmware
#   scripts/fetch-firmware.sh DIR             copy from a linux-firmware tree
#   scripts/fetch-firmware.sh --download      fetch from kernel.org (needs
#                                             network: run it on the host, or
#                                             in ./corrode shell with NET=1)
#
# Copying only reads files and writes into this directory. It does not touch
# the GPU, the driver, or anything else on the host.

set -euo pipefail

VERSION=570.144
CHIP=ga104
# GA104 shares GA102's GSP images; linux-firmware links one to the other.
FILES=(booter_load booter_unload bootloader gsp)
UPSTREAM=https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git/plain

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$here/firmware/nvidia/$CHIP/gsp"
mkdir -p "$out"

fetch_one() {
    local name="$1-$VERSION.bin" source="${2:-}"
    if [ "$source" = "--download" ]; then
        # Upstream keeps the uncompressed file; distributions compress it.
        curl -fsSL "$UPSTREAM/nvidia/ga102/gsp/$name" -o "$out/$name"
    elif [ -f "$source/nvidia/$CHIP/gsp/$name" ]; then
        cp "$source/nvidia/$CHIP/gsp/$name" "$out/$name"
    elif [ -f "$source/nvidia/$CHIP/gsp/$name.xz" ]; then
        xz -dc "$source/nvidia/$CHIP/gsp/$name.xz" > "$out/$name"
    elif [ -f "$source/nvidia/$CHIP/gsp/$name.zst" ]; then
        zstd -qdc "$source/nvidia/$CHIP/gsp/$name.zst" > "$out/$name"
    else
        echo "fetch-firmware: $name not found under $source" >&2
        return 1
    fi
    echo "fetch-firmware: $(du -h "$out/$name" | cut -f1)	$name"
}

source="${1:-/lib/firmware}"
for file in "${FILES[@]}"; do
    fetch_one "$file" "$source"
done
echo "fetch-firmware: done; the next ./corrode iso carries them"
