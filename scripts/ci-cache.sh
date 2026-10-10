#!/usr/bin/env bash
set -euo pipefail

release=''
commit=''
host=''
while IFS=' ' read -r label value; do
  case "$label" in
    release:) release="$value" ;;
    commit-hash:) commit="$value" ;;
    host:) host="$value" ;;
  esac
done < <(rustc --version --verbose)
test -n "$release" && test -n "$commit" && test -n "$host"
toolchain_key="$release-$commit-$host"
printf 'toolchain-key=%s\n' "$toolchain_key" >> "$GITHUB_OUTPUT"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
# Node cache actions need native paths, not Git Bash's /c/... spelling.
if [[ "$RUNNER_OS" == Windows ]]; then
  cargo_home="$(cygpath -m "$cargo_home")"
fi
{
  printf 'cargo-cache-paths<<CARGO_PATHS\n'
  printf '%s\n' "$cargo_home/registry/index" "$cargo_home/registry/cache" "$cargo_home/registry/src" "$cargo_home/git/db" "$cargo_home/git/checkouts"
  printf 'CARGO_PATHS\n'
} >> "$GITHUB_OUTPUT"

# Measurements leave the compiler wrapper and object-cache environment untouched.
if [[ "${ENABLE_SCCACHE:-false}" == true ]]; then
  mode=READ_ONLY
  if [[ "$CACHE_WRITABLE" == true ]]; then
    mode=READ_WRITE
  fi
  {
    printf 'RUSTC_WRAPPER=sccache\n'
    printf 'CARGO_INCREMENTAL=0\n'
    if [[ -n "${BUILDFETCH_TOKEN:-}" ]]; then
      printf 'SCCACHE_WEBDAV_ENDPOINT=https://cache.eu-central-a.buildfetch.com/sccache/hVXWOX\n'
      printf 'SCCACHE_WEBDAV_USERNAME=token-auth\n'
      printf 'SCCACHE_WEBDAV_PASSWORD=%s\n' "$BUILDFETCH_TOKEN"
      printf 'SCCACHE_WEBDAV_RW_MODE=%s\n' "$mode"
    fi
  } >> "$GITHUB_ENV"
fi
