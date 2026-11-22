#!/usr/bin/env bash
# Write dir.exe from verdigris/dir: a Windows program compiled by rustc for
# x86_64-pc-windows-msvc and linked by rust-lld (as lld-link).
#
# The toolchain has no standard library for that target and the build has
# no network, so `core` and `compiler_builtins` are compiled here from the
# toolchain's own rust-src, then the program against them. Both are nightly
# crates; RUSTC_BOOTSTRAP=1 lets this stable rustc build them, for this
# script only. Imports are `raw-dylib`, so no import library is needed.
set -euo pipefail
out=${1:?usage: dir-exe.sh OUT}
here=$(cd "$(dirname "$0")/.." && pwd)
lib=$(rustc --print sysroot)/lib/rustlib/src/rust/library
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export RUSTC_BOOTSTRAP=1
flags=(--target x86_64-pc-windows-msvc -C opt-level=s -C panic=abort)

rustc --edition 2024 --crate-type rlib --crate-name core "${flags[@]}" \
    -o "$tmp/libcore.rlib" "$lib/core/src/lib.rs"
# What its build.rs would set for this target: the C-named intrinsics,
# memcpy and the rest (there is no C runtime), and f16 support.
rustc --edition 2024 --crate-type rlib --crate-name compiler_builtins "${flags[@]}" -L "$tmp" \
    --cfg 'feature="compiler-builtins"' --cfg 'feature="unmangled-names"' \
    --cfg 'feature="mem"' --cfg 'feature="arch"' --cfg mem_unaligned --cfg f16_enabled \
    --cap-lints allow -o "$tmp/libcompiler_builtins.rlib" \
    "$lib/compiler-builtins/compiler-builtins/src/lib.rs"
# Linked as a linker links one by default: based at 0x1_4000_0000 with the
# relocations to move it, which the loader does. No C runtime.
rustc --edition 2021 --crate-type bin --crate-name dir "${flags[@]}" -L "$tmp" \
    -C linker=rust-lld -C link-arg=/ENTRY:start -C link-arg=/SUBSYSTEM:CONSOLE \
    -C link-arg=/NODEFAULTLIB -o "$tmp/dir.exe" "$here/dir/src/main.rs"
cp "$tmp/dir.exe" "$out"
