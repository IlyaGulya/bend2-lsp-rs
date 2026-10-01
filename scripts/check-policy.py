#!/usr/bin/env python3
"""Reject Rust allow/expect attributes, including conditional suppressions."""

from pathlib import Path
import sys


SUPPRESSIONS = {"allow", "expect"}
SOURCE_DIRS = ("src", "tests", "benches", "examples")
OPEN_DELIMITERS = {"(": ")", "[": "]", "{": "}"}


def raw_string_end(source: str, start: int) -> int | None:
    for prefix in ("br", "cr", "r"):
        if not source.startswith(prefix, start):
            continue
        quote = start + len(prefix)
        while quote < len(source) and source[quote] == "#":
            quote += 1
        if quote < len(source) and source[quote] == '"':
            terminator = '"' + source[start + len(prefix):quote]
            end = source.find(terminator, quote + 1)
            return len(source) if end == -1 else end + len(terminator)
    return None


def char_literal_end(source: str, start: int) -> int | None:
    """Recognize one character or escape, not a lifetime such as 'static."""
    end = start + 1
    if end >= len(source) or source[end] == "'":
        return None
    if source[end] == "\\":
        end += 1
        if end >= len(source):
            return None
        if source.startswith("u{", end):
            close = source.find("}", end + 2)
            if close == -1:
                return None
            end = close + 1
        elif source[end] == "x":
            end += 3
        else:
            end += 1
    else:
        end += 1
    return end + 1 if end < len(source) and source[end] == "'" else None


def rust_tokens(source: str) -> list[tuple[str, int]]:
    """Keep identifiers and punctuation; comments and literals cannot form attributes."""
    tokens = []
    index = 0
    while index < len(source):
        character = source[index]
        if character.isspace():
            index += 1
            continue
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            index = len(source) if end == -1 else end + 1
            continue
        if source.startswith("/*", index):
            depth = 1
            index += 2
            while index < len(source) and depth:
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
            continue
        end = raw_string_end(source, index) if character in "rbc" else None
        if end is not None:
            tokens.append(("<literal>", index))
            index = end
            continue
        if character == '"':
            end = index + 1
            while end < len(source):
                if source[end] == "\\":
                    end += 2
                elif source[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            tokens.append(("<literal>", index))
            index = end
            continue
        if character == "'":
            end = char_literal_end(source, index)
            if end is not None:
                tokens.append(("<literal>", index))
                index = end
                continue
        identifier_start = index
        if source.startswith("r#", index):
            identifier_start += 2
        if source[identifier_start:identifier_start + 1].isidentifier():
            end = identifier_start + 1
            while end < len(source) and ("a" + source[end]).isidentifier():
                end += 1
            tokens.append((source[identifier_start:end], index))
            index = end
        else:
            tokens.append((character, index))
            index += 1
    return tokens


def suppression_offsets(source: str) -> list[int]:
    tokens = rust_tokens(source)
    # Match token-tree delimiters once so nested cfg_attr arguments stay separate.
    closing = {}
    stack = []
    for index, (token, _) in enumerate(tokens):
        if token in OPEN_DELIMITERS:
            stack.append(index)
        elif token in OPEN_DELIMITERS.values():
            if stack and OPEN_DELIMITERS[tokens[stack[-1]][0]] == token:
                closing[stack.pop()] = index
            else:
                stack.clear()

    def suppresses(start: int, end: int) -> bool:
        pending = [(start, end)]
        while pending:
            start, end = pending.pop()
            if start + 1 >= end or tokens[start + 1][0] != "(":
                continue
            name = tokens[start][0]
            if name in SUPPRESSIONS:
                return True
            if name != "cfg_attr":
                continue
            close = closing.get(start + 1)
            if close is None or close >= end:
                continue
            # The first argument is a cfg predicate, not an attribute.
            argument = start + 2
            index = argument
            first_argument = True
            while index <= close:
                if index == close or tokens[index][0] == ",":
                    if not first_argument and argument < index:
                        pending.append((argument, index))
                    first_argument = False
                    argument = index + 1
                elif index in closing:
                    index = closing[index]
                index += 1
        return False

    offsets = []
    for index, (token, offset) in enumerate(tokens):
        if token != "#":
            continue
        opening = index + 1
        if opening < len(tokens) and tokens[opening][0] == "!":
            opening += 1
        if opening < len(tokens) and tokens[opening][0] == "[":
            close = closing.get(opening)
            if close is not None and suppresses(opening + 1, close):
                offsets.append(offset)
    return offsets


def main() -> int:
    root = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]
    violations = 0
    paths = []
    for directory in SOURCE_DIRS:
        source_root = root / directory
        if source_root.is_dir():
            paths.extend(sorted(source_root.rglob("*.rs")))
    build_script = root / "build.rs"
    if build_script.is_file():
        paths.append(build_script)
    for path in paths:
        source = path.read_text(encoding="utf-8")
        for offset in suppression_offsets(source):
            line = source.count("\n", 0, offset) + 1
            print(f"{path.relative_to(root)}:{line}: inline #[allow(...)]/#[expect(...)] is forbidden", file=sys.stderr)
            violations += 1

    if violations:
        print("Set exceptions centrally in Cargo.toml; do not suppress lints in Rust source.", file=sys.stderr)
        return 1
    print("Policy check passed: no inline lint suppressions.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
