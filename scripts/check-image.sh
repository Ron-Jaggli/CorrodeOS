#!/usr/bin/env bash
#
# Fail the build if any allocatable section landed outside [__kernel_start,
# __kernel_end). Those are the bounds init_paging() maps, so a section beyond
# them is present in the image, reachable by the linker, and unmapped at
# runtime -- which shows up as a page fault in whatever touches it first,
# nowhere near the cause.
#
# Addresses are compared as zero-padded 16-digit hex strings, so lexicographic
# order matches numeric order and no arithmetic is needed.

set -euo pipefail

kernel="${1:?usage: check-image.sh <kernel elf>}"

start=$(nm "$kernel" | awk '$3 == "__kernel_start" { print $1 }')
end=$(nm "$kernel" | awk '$3 == "__kernel_end" { print $1 }')

if [ -z "$start" ] || [ -z "$end" ]; then
    echo "check-image: __kernel_start/__kernel_end missing from $kernel" >&2
    exit 1
fi

stray=$(objdump -h "$kernel" | awk -v s="$start" -v e="$end" '
    /^ *[0-9]+ / { name = $2; vma = $4; pending = 1; next }
    pending && /ALLOC/ {
        if (vma >= e || vma < s) printf "  %s at 0x%s\n", name, vma
        pending = 0
        next
    }
    pending { pending = 0 }
')

if [ -n "$stray" ]; then
    echo "check-image: allocatable sections outside the mapped kernel image" >&2
    echo "             (mapped range 0x$start .. 0x$end)" >&2
    echo "$stray" >&2
    echo "             add them to kernel/linker.ld before __kernel_end" >&2
    exit 1
fi

echo "check-image: all allocatable sections within 0x$start .. 0x$end"
