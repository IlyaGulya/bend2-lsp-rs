# Rust engineering rules

## Required gate

Before considering a change complete, run:

```sh
./scripts/quality
```

The toolchain is pinned by `rust-toolchain.toml`; helper-tool versions are pinned by `scripts/install-tools.sh`.

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

## Performance-sensitive changes

Measure the baseline before changing a hot path. Keep a performance change only when the Linux Callgrind comparison in CI demonstrates the intended result or a maintainer explicitly approves the regression. Update the benchmark when the changed path is not represented. The benchmark measures source-analysis functions directly; it does not claim to measure editor latency or the complete LSP process.

The release profile uses optimization level 3, fat LTO, one codegen unit, and symbol stripping. CPU-specific code generation and PGO remain unset until deployment hardware and a representative workload are defined.

## Policy-file changes

Changes to enforcement files require the `policy-approved` label in the pull request. The `policy-integrity` workflow uses the base-branch workflow definition and does not check out or execute pull-request code. Repository administrators must also protect `main` with a ruleset requiring `quality / quality`, `performance / compare`, and `policy-integrity / protect`, requiring pull requests and review, blocking force pushes, and disallowing bypass. Those server-side rules cannot be activated from this local repository.
