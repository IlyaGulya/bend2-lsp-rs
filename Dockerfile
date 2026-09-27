# syntax=docker/dockerfile:1.7
FROM rust:1.98.1-bookworm

ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH="/usr/local/cargo/bin:${PATH}"

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential ca-certificates libssl-dev pkg-config valgrind \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add rustfmt clippy llvm-tools-preview --toolchain 1.98.1

RUN --mount=type=cache,id=bend2-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=bend2-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=bend2-tool-target,target=/tmp/cargo-target \
    export CARGO_TARGET_DIR=/tmp/cargo-target \
    && cargo +1.98.1 install --locked --version 0.9.131 cargo-nextest \
    && cargo +1.98.1 install --locked --version 0.20.2 cargo-deny \
    && cargo +1.98.1 install --locked --version 0.6.45 cargo-hack \
    && cargo +1.98.1 install --locked --version 0.16.1 iai-callgrind-runner

WORKDIR /workspace
CMD ["sleep", "infinity"]
