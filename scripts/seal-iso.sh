#!/usr/bin/env bash
# Make a boot ISO from TREE that boots only what was built.
#
# Every file Limine loads -- the kernel and each module -- gets its BLAKE2B
# hash after its path in TREE/boot/limine/limine.conf (`path#hash`), and
# Limine refuses one that does not match. The configuration's own hash is
# then enrolled into each Limine executable on the ISO, so the hashes
# cannot be edited out either: a changed config is refused too. What is
# left to trust is Limine itself, which Secure Boot signs on real hardware.
#
# Used for the ISO and for the test ISOs made from copies of its tree.
# With --pack-only it only packs TREE as it is: verify-test uses that to
# make ISOs whose files no longer match their hashes.
set -euo pipefail
pack_only=
if [ "${1:-}" = --pack-only ]; then pack_only=1; shift; fi
tree=${1:?usage: seal-iso.sh [--pack-only] TREE OUT}
out=${2:?usage: seal-iso.sh [--pack-only] TREE OUT}
limine=${LIMINE_DIR:-/opt/limine}
conf=$tree/boot/limine/limine.conf

if [ -z "$pack_only" ]; then

# Hash every path, dropping any hash a copied tree already carried.
tmp=$conf.new
while IFS= read -r line; do
    if [[ $line =~ ^([[:space:]]*(kernel_path|module_path):[[:space:]]*boot\(\):)([^#]*)(#.*)?$ ]]; then
        path=${BASH_REMATCH[3]}
        hash=$(b2sum "$tree$path" | cut -d' ' -f1)
        line="${BASH_REMATCH[1]}$path#$hash"
    fi
    printf '%s\n' "$line"
done < "$conf" > "$tmp"
mv "$tmp" "$conf"
config_hash=$(b2sum "$conf" | cut -d' ' -f1)

# Fresh executables each time (enrolling twice would fail), with the hash.
cp "$limine/limine-bios.sys" "$limine/limine-bios-cd.bin" "$limine/limine-uefi-cd.bin" "$tree/boot/limine/"
cp "$limine/BOOTX64.EFI" "$limine/BOOTIA32.EFI" "$tree/EFI/BOOT/"
for exe in "$tree/boot/limine/limine-bios.sys" "$tree/EFI/BOOT/BOOTX64.EFI" "$tree/EFI/BOOT/BOOTIA32.EFI"; do
    "$limine/limine" enroll-config --quiet "$exe" "$config_hash"
done
# UEFI firmware boots the CD from this FAT image, not from EFI/BOOT.
mcopy -o -i "$tree/boot/limine/limine-uefi-cd.bin" "$tree/EFI/BOOT/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI
mcopy -o -i "$tree/boot/limine/limine-uefi-cd.bin" "$tree/EFI/BOOT/BOOTIA32.EFI" ::/EFI/BOOT/BOOTIA32.EFI
fi

xorriso -as mkisofs -quiet -R -r -J \
    -b boot/limine/limine-bios-cd.bin \
    -no-emul-boot -boot-load-size 4 -boot-info-table \
    --efi-boot boot/limine/limine-uefi-cd.bin \
    -efi-boot-part --efi-boot-image \
    --protective-msdos-label \
    "$tree" -o "$out"
"$limine/limine" bios-install "$out" > /dev/null
