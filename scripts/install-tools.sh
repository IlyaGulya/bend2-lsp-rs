#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Release-only generator; ordinary quality-tool installation remains unchanged.
if [[ "${1:-}" == "dist" ]]; then
  # The upstream 0.33.0 release is not published on crates.io. The global job
  # uses Linux x86_64; pin both the official binary release and its exact bytes.
  test "$(uname -s)" = Linux
  test "$(uname -m)" = x86_64
  temporary="$(mktemp -d)"
  trap 'rm -rf "$temporary"' EXIT
  archive="cargo-dist-x86_64-unknown-linux-gnu.tar.xz"
  curl -fsSL "https://github.com/axodotdev/cargo-dist/releases/download/v0.33.0/$archive" --output "$temporary/$archive"
  printf '%s  %s\n' '4b3f0a5f0ebbdb798f6db649d01b32ba1518376b6f7a0502b7d92b75cc2c8293' "$temporary/$archive" | sha256sum --check -
  tar -xJf "$temporary/$archive" -C "$temporary"
  mkdir -p "$HOME/.cargo/bin"
  install -m 755 "$temporary/cargo-dist-x86_64-unknown-linux-gnu/dist" "$HOME/.cargo/bin/dist"
  exit 0
fi

cargo install --locked --version 0.9.131 cargo-nextest
cargo install --locked --version 0.20.2 cargo-deny
cargo install --locked --version 0.6.45 cargo-hack
cargo install --locked --version 1.30.1 zizmor
go install -ldflags='-X main.version=1.5.6' github.com/suzuki-shunsuke/ghalint/cmd/ghalint@v1.5.6
go install github.com/rhysd/actionlint/cmd/actionlint@v1.7.12
