#!/usr/bin/env bash

check_tool_version() {
    local tool="$1" expected="$2" actual="$3" version=""
    actual="${actual%%$'\n'*}"
    if [[ "$actual" =~ (^|[[:space:]])([0-9]+\.[0-9]+\.[0-9]+[^[:space:]]*)($|[[:space:]]) ]]; then
        version="${BASH_REMATCH[2]}"
    fi
    if [[ "$version" != "$expected" ]]; then
        printf 'Expected %s version %s; got: %s\n' "$tool" "$expected" "$actual" >&2
        printf 'Run ./scripts/install-tools.sh\n' >&2
        return 1
    fi
}
