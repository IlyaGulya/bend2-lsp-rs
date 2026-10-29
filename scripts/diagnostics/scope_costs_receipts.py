#!/usr/bin/env python3
"""Validate unpaired AC53.6/AC53.7 scope-cost receipts from the hosted runner.

This is deliberately not a pass/fail performance policy. It only proves that
every diagnostic row executed, that fixture parameters and source SHA-256
identities match the intended construction, and that each row has a genuine
Callgrind report with numeric summary/totals values for all nine cache-simulated
event counters, bound to the expected benchmark binary and callgrind version.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

PINNED_SUT_SHA = "9eb86a196b1c183a3b8798bf61a7531a91c88ff4"
ALL_NINE = ("Ir", "Dr", "Dw", "I1mr", "D1mr", "D1mw", "ILmr", "DLmr", "DLmw")

# row -> nested lexical depth
NESTED_ROWS = {
    "nested_depth_8": 8,
    "nested_depth_32": 32,
    "nested_depth_128": 128,
}

# row -> (dependencies, declarations_per_dependency, roots, retained_reader)
RELEASE_ROWS = {
    "close_declarations_100": (1, 100, 1, False),
    "close_declarations_1000": (1, 1000, 1, False),
    "close_declarations_10000": (1, 10000, 1, False),
    "close_files_2": (2, 100, 1, False),
    "close_files_8": (8, 100, 1, False),
    "close_files_32": (32, 100, 1, False),
    "close_views_1": (1, 100, 1, False),
    "close_views_2": (1, 100, 2, False),
    "close_views_8": (1, 100, 8, False),
    "close_retained_reader_10000": (1, 10000, 1, True),
}

# row -> (annotated function, bench id)
RAW_IDENTITIES = {
    "nested_depth_8": ("cold_snapshot_nested_scope", "depth_8"),
    "nested_depth_32": ("cold_snapshot_nested_scope", "depth_32"),
    "nested_depth_128": ("cold_snapshot_nested_scope", "depth_128"),
    "close_declarations_100": ("published_semantic_view_close_orphan_release", "declarations_100"),
    "close_declarations_1000": (
        "published_semantic_view_close_orphan_release",
        "declarations_1000",
    ),
    "close_declarations_10000": (
        "published_semantic_view_close_orphan_release",
        "declarations_10000",
    ),
    "close_files_2": ("published_semantic_view_close_orphan_release", "files_2"),
    "close_files_8": ("published_semantic_view_close_orphan_release", "files_8"),
    "close_files_32": ("published_semantic_view_close_orphan_release", "files_32"),
    "close_views_1": ("published_semantic_view_close_orphan_release", "views_1"),
    "close_views_2": ("published_semantic_view_close_orphan_release", "views_2"),
    "close_views_8": ("published_semantic_view_close_orphan_release", "views_8"),
    "close_retained_reader_10000": (
        "published_semantic_view_close_orphan_release_retained_reader",
        "retained_10000",
    ),
}


def nested_source(depth: int) -> str:
    binders = "".join(f"x{index} => " for index in range(depth))
    return f"def nested(value: U32) -> U32:\n  {binders}value\n"


def dependency_source(declarations: int) -> str:
    source = "def target(value: U32) -> U32:\n  value\n"
    for declaration in range(declarations):
        source += f"def unused_{declaration}(value: U32) -> U32:\n  value\n"
    return source


def published_root_source(dependencies: int) -> str:
    source = "".join(
        f"import ./d{index:03}.bend as D{index:03}\n" for index in range(dependencies)
    )
    source += "def main(value: U32) -> U32:\n  "
    source += "".join(f"D{index:03}.target!(" for index in range(dependencies))
    source += "value"
    source += ")" * dependencies
    return source + "\n"


def parse_receipts(path: Path, errors: list[str]) -> dict[str, dict[str, str]]:
    rows: dict[str, dict[str, str]] = {}
    if not path.is_file():
        errors.append(f"missing fixture receipt file: {path}")
        return rows
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        fields: dict[str, str] = {}
        row = None
        for token in line.split():
            key, separator, value = token.partition("=")
            if not separator or not key or not value:
                errors.append(f"{path}:{number}: malformed token {token!r}")
                return rows
            if key == "row":
                row = value
            else:
                fields[key] = value
        if row is None:
            errors.append(f"{path}:{number}: receipt has no row identity")
            continue
        if row in rows and rows[row] != fields:
            errors.append(f"{path}:{number}: inconsistent duplicate receipt for {row}")
            continue
        rows[row] = fields
    return rows


def integer(row: str, fields: dict[str, str], key: str, errors: list[str]) -> int:
    value = fields.get(key)
    try:
        return int(value)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        errors.append(f"{row}: {key} is missing or not an integer ({value!r})")
        return -1


def parse_profile(profile: Path, errors: list[str]) -> dict[str, object] | None:
    try:
        lines = profile.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        errors.append(f"{profile}: unreadable Callgrind report ({error})")
        return None
    events: tuple[str, ...] | None = None
    creator: str | None = None
    cmd: str | None = None
    summaries: list[list[str]] = []
    totals: list[list[str]] = []
    for line in lines:
        if line.startswith("events:"):
            events = tuple(line.removeprefix("events:").split())
        elif line.startswith("summary:"):
            summaries.append(line.removeprefix("summary:").split())
        elif line.startswith("totals:"):
            totals.append(line.removeprefix("totals:").split())
        elif line.startswith("creator:"):
            creator = line.removeprefix("creator:").strip()
        elif line.startswith("cmd:"):
            cmd = line.removeprefix("cmd:").strip()
    if events is None:
        errors.append(f"{profile}: no Callgrind events header")
        return None
    if len(events) != len(ALL_NINE) or set(events) != set(ALL_NINE):
        errors.append(f"{profile}: expected all nine counters {ALL_NINE}, got {events}")
        return None

    def counters(kind: str, rows: list[list[str]]) -> list[list[int]] | None:
        if not rows:
            errors.append(f"{profile}: no Callgrind {kind} counters")
            return None
        parsed: list[list[int]] = []
        for row in rows:
            if len(row) != len(events):
                errors.append(
                    f"{profile}: {kind} counter count {len(row)} does not match {len(events)}"
                )
                return None
            try:
                values = [int(value) for value in row]
            except ValueError:
                errors.append(f"{profile}: non-numeric {kind} counters {row}")
                return None
            if any(value < 0 for value in values):
                errors.append(f"{profile}: negative {kind} counters {row}")
                return None
            parsed.append(values)
        return parsed

    summary_rows = counters("summary", summaries)
    total_rows = counters("totals", totals)
    if summary_rows is None or total_rows is None:
        return None
    # A single Callgrind part reports matching summary/totals counters; with
    # separately reported parts the totals aggregate the per-part summaries.
    expected_total = (
        summary_rows[0]
        if len(summary_rows) == 1
        else [sum(values) for values in zip(*summary_rows)]
    )
    if total_rows[-1] != expected_total:
        errors.append(f"{profile}: summary and totals counters disagree")
        return None
    summary = expected_total
    mapped = dict(zip(events, summary))
    if mapped["Ir"] <= 0:
        errors.append(f"{profile}: instruction counter must be positive")
        return None
    if creator is None or not creator.startswith("callgrind-3.27.1"):
        errors.append(f"{profile}: unexpected Callgrind creator {creator!r}")
        return None
    if not cmd or "analysis" not in cmd:
        errors.append(f"{profile}: Callgrind command does not identify the bench binary")
        return None
    return {"events": list(events), "counters": mapped, "creator": creator}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipts", required=True, type=Path)
    parser.add_argument("--iai-root", required=True, type=Path)
    parser.add_argument("--log", required=True, type=Path)
    parser.add_argument("--metadata", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    errors: list[str] = []
    receipt_rows = parse_receipts(args.receipts, errors)

    expected_rows = set(NESTED_ROWS) | set(RELEASE_ROWS)
    missing_rows = sorted(expected_rows - set(receipt_rows))
    unexpected_rows = sorted(set(receipt_rows) - expected_rows)
    if missing_rows:
        errors.append(f"diagnostic rows that never executed: {missing_rows}")
    if unexpected_rows:
        errors.append(f"unexpected diagnostic receipts: {unexpected_rows}")

    rows_report: list[dict[str, object]] = []
    for row in sorted(expected_rows & set(receipt_rows)):
        fields = receipt_rows[row]
        entry: dict[str, object] = {"row": row, "facts": fields}
        if row in NESTED_ROWS:
            depth = NESTED_ROWS[row]
            scopes = integer(row, fields, "scopes", errors)
            lexical_depth = integer(row, fields, "lexical_depth", errors)
            bindings = integer(row, fields, "bindings", errors)
            symbols = integer(row, fields, "symbols", errors)
            source_bytes = integer(row, fields, "source_bytes", errors)
            expected_bytes = len(nested_source(depth))
            if fields.get("kind") != "nested_scope":
                errors.append(f"{row}: expected nested_scope kind, got {fields.get('kind')!r}")
            if integer(row, fields, "depth", errors) != depth:
                errors.append(f"{row}: recorded depth does not match the row")
            if scopes < depth + 1:
                errors.append(f"{row}: expected at least {depth + 1} scopes, got {scopes}")
            if lexical_depth != depth + 1:
                errors.append(f"{row}: expected lexical depth {depth + 1}, got {lexical_depth}")
            if bindings < depth + 1:
                errors.append(f"{row}: expected at least {depth + 1} bindings, got {bindings}")
            if symbols != 1:
                errors.append(f"{row}: expected one indexed declaration, got {symbols}")
            if source_bytes != expected_bytes:
                errors.append(
                    f"{row}: source bytes {source_bytes} do not match {expected_bytes}"
                )
            expected_source_sha = hashlib.sha256(nested_source(depth).encode()).hexdigest()
            if fields.get("source_sha256") != expected_source_sha:
                errors.append(
                    f"{row}: source sha256 {fields.get('source_sha256')!r} does not match "
                    f"{expected_source_sha}"
                )
            entry["source_sha256"] = expected_source_sha
            entry["parameters_sha256"] = hashlib.sha256(
                f"depth={depth}\n".encode()
            ).hexdigest()
            entry["curve"] = "nested_lexical_scope"
            entry["depth"] = depth
        else:
            dependencies, declarations, roots, retained = RELEASE_ROWS[row]
            source_bytes = integer(row, fields, "source_bytes", errors)
            expected_bytes = len(published_root_source(dependencies)) + (
                dependencies * len(dependency_source(declarations))
            )
            documents = integer(row, fields, "documents", errors)
            view_declarations = integer(row, fields, "view_declarations", errors)
            checks = {
                "dependencies": dependencies,
                "declarations_per_dependency": declarations,
                "roots": roots,
                "views_published": roots,
                "files": dependencies + 1,
                "documents": dependencies + 1,
                "view_declarations": dependencies * (declarations + 1) + 1,
                "source_bytes": expected_bytes,
                "retained_reader": "1" if retained else "0",
            }
            if fields.get("kind") != "published_view_close_release":
                errors.append(
                    f"{row}: expected published_view_close_release kind, "
                    f"got {fields.get('kind')!r}"
                )
            if fields.get("cold_build") != "outside_measurement":
                errors.append(
                    f"{row}: cold construction must stay outside the measured close path"
                )
            for key, expected in checks.items():
                if fields.get(key) != str(expected):
                    errors.append(
                        f"{row}: {key}={fields.get(key)!r} does not match {expected!r}"
                    )
            if documents != dependencies + 1:
                errors.append(f"{row}: expected {dependencies + 1} documents, got {documents}")
            if view_declarations != dependencies * (declarations + 1) + 1:
                errors.append(
                    f"{row}: expected {dependencies * (declarations + 1) + 1} published "
                    f"declarations, got {view_declarations}"
                )
            if source_bytes != expected_bytes:
                errors.append(
                    f"{row}: source bytes {source_bytes} do not match {expected_bytes}"
                )
            expected_root_sha = hashlib.sha256(
                published_root_source(dependencies).encode()
            ).hexdigest()
            expected_dependency_sha = hashlib.sha256(
                dependency_source(declarations).encode()
            ).hexdigest()
            if fields.get("root_source_sha256") != expected_root_sha:
                errors.append(
                    f"{row}: root source sha256 does not match {expected_root_sha}"
                )
            if fields.get("dependency_source_sha256") != expected_dependency_sha:
                errors.append(
                    f"{row}: dependency source sha256 does not match "
                    f"{expected_dependency_sha}"
                )
            entry["root_source_sha256"] = expected_root_sha
            entry["dependency_source_sha256"] = expected_dependency_sha
            entry["parameters_sha256"] = hashlib.sha256(
                (
                    f"dependencies={dependencies}\n"
                    f"declarations_per_dependency={declarations}\n"
                    f"roots={roots}\n"
                    f"retained_reader={int(retained)}\n"
                ).encode()
            ).hexdigest()
            entry["curve"] = (
                "retained_reader"
                if retained
                else "published_views"
                if roots > 1 or row.startswith("close_views_")
                else "declarations"
                if row.startswith("close_declarations")
                else "files"
            )
            entry["retained_reader"] = retained

        function, bench_id = RAW_IDENTITIES[row]
        prefix = f"callgrind.{function}.{bench_id}.out"
        profiles = sorted(
            path
            for path in args.iai_root.rglob("*")
            if path.is_file() and path.name.startswith(prefix)
        )
        verified: list[dict[str, object]] = []
        for profile in profiles:
            parsed = parse_profile(profile, errors)
            if parsed is None:
                continue
            verified.append(
                {
                    "path": str(profile.relative_to(args.iai_root)),
                    "sha256": hashlib.sha256(profile.read_bytes()).hexdigest(),
                    "creator": parsed["creator"],
                    "counters": parsed["counters"],
                }
            )
        if not verified:
            errors.append(f"{row}: no raw Callgrind report with all nine counters")
        entry["raw_reports"] = verified
        rows_report.append(entry)

    log_text = ""
    if not args.log.is_file():
        errors.append(f"missing Callgrind log: {args.log}")
    else:
        log_text = args.log.read_text(encoding="utf-8", errors="replace")
        if not log_text.strip():
            errors.append(f"empty Callgrind log: {args.log}")
        for function in {function for function, _ in RAW_IDENTITIES.values()}:
            if function not in log_text:
                errors.append(f"run log does not mention {function}")

    metadata: dict[str, object] = {}
    if not args.metadata.is_file():
        errors.append(f"missing run metadata: {args.metadata}")
    else:
        metadata = json.loads(args.metadata.read_text(encoding="utf-8"))
        if metadata.get("sut_source_sha") != PINNED_SUT_SHA:
            errors.append(
                "immutable SUT source must be "
                f"{PINNED_SUT_SHA}, got {metadata.get('sut_source_sha')!r}"
            )
        for key in ("head_sha", "runner", "rustc", "valgrind", "toolchain"):
            if not metadata.get(key):
                errors.append(f"run metadata is missing {key}")

    report = {
        "format_version": 1,
        "kind": "diagnostic_unpaired_semantic_scope_costs",
        "promotable_to_paired_gate": False,
        "sut_source_sha": PINNED_SUT_SHA,
        "metadata": metadata,
        "counters": list(ALL_NINE),
        "rows": rows_report,
        "complete": not errors,
        "errors": errors,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )

    summary = args.output.with_suffix(".md")
    lines = [
        "# Diagnostic semantic scope and published-view close/orphan-release costs",
        "",
        "Diagnostic and unpaired. These rows are not part of any baseline, "
        "threshold, or paired admission policy.",
        "",
        "Release rows measure `close_document` of every open root that owns a "
        "published view, followed by the empty reachability completion that "
        "releases retired graph ownership. Snapshot construction, parsing, "
        "capture, and view build/publish happen only in setup.",
        "",
        f"- Immutable SUT source: `{PINNED_SUT_SHA}`",
        f"- Diagnostic head: `{metadata.get('head_sha')}`",
        f"- Runner: `{metadata.get('runner')}`",
        f"- Valgrind: `{metadata.get('valgrind')}`",
        f"- Toolchain: `{metadata.get('toolchain')}`",
        f"- Counters: {', '.join(ALL_NINE)}",
        "",
        "| Row | Curve | Parameter | Source bytes | Raw all-9 reports |",
        "| --- | --- | --- | ---: | ---: |",
    ]
    for entry in rows_report:
        facts = entry["facts"]
        if "depth" in entry:
            parameter = f"depth={entry['depth']}"
        else:
            parameter = (
                f"deps={facts.get('dependencies')} "
                f"decls/dep={facts.get('declarations_per_dependency')} "
                f"roots={facts.get('roots')}"
            )
        lines.append(
            f"| `{entry['row']}` | {entry.get('curve')} | {parameter} | "
            f"{facts.get('source_bytes')} | {len(entry.get('raw_reports', []))} |"
        )
    lines.extend(
        [
            "",
            f"Complete: `{'yes' if not errors else 'no'}`",
            "",
        ]
    )
    if errors:
        lines.extend(["## Missing or inconsistent receipts", ""])
        lines.extend(f"- {error}" for error in errors)
        lines.append("")
    summary.write_text("\n".join(lines), encoding="utf-8")

    for error in errors:
        print(f"scope diagnostics receipt error: {error}", file=sys.stderr)
    print(f"diagnostic receipt rows: {len(rows_report)}/{len(expected_rows)}")
    return 0 if not errors else 1


if __name__ == "__main__":
    raise SystemExit(main())
