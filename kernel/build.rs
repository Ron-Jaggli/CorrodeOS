fn main() {
    // Cargo has no idea the linker script exists -- it is passed through
    // rustflags in .cargo/config.toml. Without this, editing linker.ld does not
    // trigger a relink and you debug a stale binary.
    println!("cargo:rerun-if-changed=linker.ld");
}
