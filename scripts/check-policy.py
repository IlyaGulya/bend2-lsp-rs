#!/usr/bin/env python3
"""Reject source-level Clippy allow/expect attributes."""

from pathlib import Path
import re
import sys


SUPPRESSION = re.compile(r"#\s*!?\s*\[\s*(?:allow|expect)\s*\(", re.MULTILINE)
SOURCE_DIRS = ("src", "tests", "benches", "examples")


def main() -> int:
    root = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]
    violations = 0
    for directory in SOURCE_DIRS:
        source_root = root / directory
        if not source_root.is_dir():
            continue
        for path in sorted(source_root.rglob("*.rs")):
            source = path.read_text(encoding="utf-8")
            for match in SUPPRESSION.finditer(source):
                line = source.count("\n", 0, match.start()) + 1
                print(f"{path.relative_to(root)}:{line}: inline #[allow(...)]/#[expect(...)] is forbidden", file=sys.stderr)
                violations += 1

    build_script = root / "build.rs"
    if build_script.is_file():
        source = build_script.read_text(encoding="utf-8")
        for match in SUPPRESSION.finditer(source):
            line = source.count("\n", 0, match.start()) + 1
            print(f"build.rs:{line}: inline #[allow(...)]/#[expect(...)] is forbidden", file=sys.stderr)
            violations += 1

    if violations:
        print("Set exceptions centrally in Cargo.toml; do not suppress lints in Rust source.", file=sys.stderr)
        return 1
    print("Policy check passed: no inline lint suppressions.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
