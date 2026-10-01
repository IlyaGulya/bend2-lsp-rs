# Rust engineering rules

## Required gate

Before considering a change complete, run:

```sh
./scripts/quality
```

The toolchain is pinned by `rust-toolchain.toml`; helper-tool versions are pinned by `scripts/install-tools.sh`.

GitHub Actions checks are mandatory in `scripts/quality`: pinned `actionlint`,
`ghalint`, and `zizmor` validate workflow syntax, permissions, action pins,
credential persistence, secret scope, and injection risks. `pull_request_target`
is forbidden. Do not add broad audit exclusions or expose privileged tokens to
pull-request code. Trusted `workflow_run` jobs must validate repository identity,
event, branch, and exact source SHA before privileged operations. Privileged
jobs must never execute PR code or consume PR artifacts.

## Policy boundaries

Never:

- add inline `#[allow(...)]` or `#[expect(...)]` attributes;
- weaken Cargo lints, the quality script, dependency policy, test retry policy, or CI checks to silence a failure;
- disable, delete, or reduce a test to make a change pass;
- change benchmark metrics, comparison thresholds, or baseline workflow without maintainer approval;
- add `unwrap()` or `expect()` in production code;
- use `unsafe` (the package forbids it);
- use `target-cpu=native` or commit PGO data without a production CPU/workload contract.

Central lint-policy exceptions belong in the workspace manifest with a concrete reason. The current `deprecated` exception is limited to LSP compatibility fields required by `tower-lsp 0.20`.

## Persistent Docker workflow

Use the `rust` service in `compose.yaml` for Linux checks and Callgrind. Build it initially with `docker compose up -d --build rust`; for routine work, run commands with `docker compose exec rust ...` and retain the named Cargo registry, Git, and target volumes. Pause and resume with `docker compose stop rust` / `docker compose start rust`. Do not use ephemeral `docker run --rm`, `docker compose down -v`, or volume pruning for routine runs. Rebuild only when the container definition or tool image changes.

## Performance-sensitive changes

Measure the baseline before changing a hot path. Keep a performance change only when the Linux Callgrind comparison in CI demonstrates the intended result or a maintainer explicitly approves the regression. Update the benchmark when the changed path is not represented. The benchmark measures source-analysis functions directly; it does not claim to measure editor latency or the complete LSP process.

The release profile uses optimization level 3, fat LTO, one codegen unit, and symbol stripping. CPU-specific code generation and PGO remain unset until deployment hardware and a representative workload are defined.

## Snapshot/index-only feature queries

LSP feature handlers and warm analysis queries MUST use immutable
`DocumentSnapshot` data; workspace-wide and cross-file queries MUST use
`Workspace` indexes. New semantic information MUST be added to
`SyntaxIndex`, `OccurrenceIndex`, workspace indexes, or an equivalent
compact indexed structure during cold snapshot/index construction. A feature
query MUST NOT repeat a full-source semantic scan over `&str`.

Full-source lexical or semantic scans are allowed during cold snapshot/index
construction. Prepare data there for reuse by warm queries.

Keep the formatter on its separate lexical scanner. Do not combine it with the
analysis scanner until exact-equivalence tests and benchmarks prove that the
shared scanner preserves formatter behavior and has an acceptable tradeoff.

Measure and gate cold snapshot construction separately from warm analysis
queries. The `cold_snapshot_build_*` and `*_warm` benchmarks in
[`benches/analysis.rs`](benches/analysis.rs) exercise these paths. The active
Callgrind regression policy is in
[`docs/performance-policy.md`](docs/performance-policy.md). New LSP feature
work MUST follow this invariant.

Forbidden:

- Add a hover, completion, references, inlay-hint, code-lens, or hierarchy
  handler that loops over every byte or character in the full source.
- Reparse declarations, parameters, identifiers, comments, or imports from
  `&str` in a feature query when the snapshot or workspace indexes already
  contain that information.
- Rebuild semantic representations during each warm query when they can be
  prepared once during snapshot construction.

Allowed:

- Look up `FileId`, `SymbolId`, `NameId`, or `TextRange` through existing
  indexes.
- Add compact metadata to `SyntaxIndex`, `OccurrenceIndex`, or workspace
  indexes during cold construction.
- Convert UTF-16 positions through `LineIndex`.
- Resolve workspace dependencies through import and reverse-import indexes.
- Keep the formatter on its separate scanner under the exception above.
- Scan a bounded local slice when this avoids a second full-document semantic
  pass and benchmark evidence supports the tradeoff.
