# syntax=docker/dockerfile:1.7
FROM golang:1.26.3-bookworm AS workflow-tools
RUN go install -ldflags='-X main.version=1.5.6' github.com/suzuki-shunsuke/ghalint/cmd/ghalint@v1.5.6 \
    && go install github.com/rhysd/actionlint/cmd/actionlint@v1.7.12

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
RUN --mount=type=cache,id=bend2-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=bend2-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=bend2-tool-target,target=/tmp/cargo-target \
    CARGO_TARGET_DIR=/tmp/cargo-target cargo +1.98.1 install --locked --version 1.30.1 zizmor

COPY --from=workflow-tools /go/bin/ghalint /go/bin/actionlint /usr/local/cargo/bin/

WORKDIR /workspace
CMD ["sleep", "infinity"]
