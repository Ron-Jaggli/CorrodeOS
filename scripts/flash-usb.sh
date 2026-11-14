#!/usr/bin/env bash
#
# Write the real-hardware ISO to a USB stick. Runs on the HOST, not in the
# build container -- it is the one script here that touches a real disk, so
# it checks hard before it does.
#
#   scripts/flash-usb.sh                 list the removable disks it would accept
#   scripts/flash-usb.sh /dev/sdX        build build/corrode-hw.iso, then write it
#
# The ISO is the normal one built with FEATURES=hw-log: the boot log is drawn
# on screen as well as COM1, since a desktop has no serial cable, and the boot
# stops after the NVIDIA stage so the desktop does not paint over it. To see
# what the stick will show without the hardware:
#
#   FEATURES=hw-log ./corrode screenshot     then look at build/frame.png
#
#   NO_BUILD=1 scripts/flash-usb.sh /dev/sdX   write the ISO already built
#
# EVERYTHING ON THE STICK IS ERASED.
#
# Refuses: anything that is not a whole disk, anything not removable or not on
# USB, the disk holding / or /var/mnt/games or any other mounted filesystem it
# could not unmount, and anything bigger than 256 GiB (a stick that size is
# rare; an external drive full of stuff is not). Then it asks you to type the
# device name back before writing.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ISO="$PROJECT_DIR/build/corrode-hw.iso"
MAX_BYTES=$((256 * 1024 * 1024 * 1024))

die() { echo "flash-usb: $*" >&2; exit 1; }

candidates() {
    lsblk -dpno NAME,RM,TRAN,SIZE,MODEL | awk '$2 == 1 || $3 == "usb"'
}

if [ $# -eq 0 ]; then
    echo "Removable / USB disks:"
    candidates || true
    echo
    echo "usage: $0 /dev/sdX"
    exit 0
fi

DEV="$1"
[ -b "$DEV" ] || die "$DEV is not a block device"
[ "$(lsblk -dno TYPE "$DEV")" = "disk" ] || die "$DEV is a partition or not a disk; give the whole disk, e.g. /dev/sdb not /dev/sdb1"

RM="$(lsblk -dno RM "$DEV" | tr -d ' ')"
TRAN="$(lsblk -dno TRAN "$DEV" | tr -d ' ')"
[ "$RM" = "1" ] || [ "$TRAN" = "usb" ] || die "$DEV is neither removable nor on USB -- refusing"

SIZE="$(lsblk -dbno SIZE "$DEV" | tr -d ' ')"
[ "$SIZE" -le "$MAX_BYTES" ] || die "$DEV is $((SIZE >> 30)) GiB; that is not a boot stick -- refusing"

# The disks the running system lives on. findmnt gives the partition; its
# parent kernel name is the disk.
for mnt in / /var/mnt/games /home /boot; do
    src="$(findmnt -no SOURCE "$mnt" 2>/dev/null | sed 's/\[.*//')" || continue
    [ -n "$src" ] || continue
    for parent in $(lsblk -nso PKNAME "$src" 2>/dev/null; lsblk -dno PKNAME "$src" 2>/dev/null); do
        [ "/dev/$parent" = "$DEV" ] && die "$DEV holds $mnt -- refusing"
    done
done

if [ "${NO_BUILD:-0}" != "1" ]; then
    echo "flash-usb: building $ISO"
    FEATURES=hw-log "$PROJECT_DIR/corrode" iso
    cp "$PROJECT_DIR/build/corrode.iso" "$ISO"
    echo "flash-usb: the next plain ./corrode build puts the normal kernel back"
fi
[ -s "$ISO" ] || die "$ISO is missing; run without NO_BUILD=1"

# The firmware is what the NVIDIA stage reads; without it the stick still
# boots but only the card half runs.
if ! ls "$PROJECT_DIR"/firmware/nvidia/ga104/gsp/*.bin >/dev/null 2>&1; then
    echo "flash-usb: warning: no GSP firmware in firmware/ -- run scripts/fetch-firmware.sh first if you want it on the stick"
fi

echo
lsblk -po NAME,SIZE,MODEL,MOUNTPOINTS "$DEV"
echo
echo "About to ERASE $DEV ($((SIZE >> 20)) MiB, $(lsblk -dno MODEL "$DEV" | sed 's/ *$//')) and write $(basename "$ISO") ($(( $(stat -c %s "$ISO") >> 20 )) MiB)."
read -r -p "Type the device name ($DEV) to go ahead: " answer
[ "$answer" = "$DEV" ] || die "did not match; nothing written"

# Unmount anything the desktop auto-mounted from the stick.
for part in $(lsblk -lnpo NAME "$DEV"); do
    if findmnt -rno TARGET -S "$part" >/dev/null 2>&1; then
        echo "flash-usb: unmounting $part"
        udisksctl unmount -b "$part" >/dev/null 2>&1 || sudo umount "$part" \
            || die "could not unmount $part; nothing written"
    fi
done

sudo dd if="$ISO" of="$DEV" bs=4M conv=fsync oflag=direct status=progress
sync
echo
echo "flash-usb: done. Boot it with Secure Boot off, from the UEFI boot menu."
