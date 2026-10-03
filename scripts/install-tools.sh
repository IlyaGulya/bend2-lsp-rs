#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Release-only generator; ordinary quality-tool installation remains unchanged.
if [[ "${1:-}" == "dist" ]]; then
  cargo install --locked --version 0.33.0 cargo-dist
  exit 0
fi

cargo install --locked --version 0.9.131 cargo-nextest
cargo install --locked --version 0.20.2 cargo-deny
cargo install --locked --version 0.6.45 cargo-hack
cargo install --locked --version 1.30.1 zizmor
go install -ldflags='-X main.version=1.5.6' github.com/suzuki-shunsuke/ghalint/cmd/ghalint@v1.5.6
go install github.com/rhysd/actionlint/cmd/actionlint@v1.7.12
