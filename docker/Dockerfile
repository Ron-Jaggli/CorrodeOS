FROM docker.io/library/rust:1-slim-bookworm

# Everything Corrode needs to build and boot lives in here. The host stays clean.
RUN apt-get update && apt-get install -y --no-install-recommends \
        make \
        git \
        ca-certificates \
        xorriso \
        mtools \
        qemu-system-x86 \
        ovmf \
        netpbm \
    && rm -rf /var/lib/apt/lists/*

# Bare-metal target. Tier 2, so core/alloc ship precompiled -- no build-std, stable rustc.
RUN rustup target add x86_64-unknown-none \
    && rustup component add rust-src llvm-tools clippy rustfmt

# Limine binary release + the `limine` host utility that stamps the ISO bootable.
ARG LIMINE_BRANCH=v8.x-binary
RUN git clone https://github.com/limine-bootloader/limine.git \
        --branch=${LIMINE_BRANCH} --depth=1 /opt/limine \
    && make -C /opt/limine \
    && rm -rf /opt/limine/.git
ENV LIMINE_DIR=/opt/limine

# Cargo writes here; bind-mounted source stays the only writable project path.
ENV CARGO_HOME=/usr/local/cargo
WORKDIR /src
