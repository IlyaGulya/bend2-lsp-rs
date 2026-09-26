#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo install --locked --version 0.9.131 cargo-nextest
cargo install --locked --version 0.20.2 cargo-deny
cargo install --locked --version 0.6.45 cargo-hack
