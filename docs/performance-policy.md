# Performance regression policy

## Scope

The project benchmarks selected source-analysis functions directly because the
package is a binary-only crate. `benches/analysis.rs` includes `src/analysis.rs`
as a private module, so benchmark code can call production functions without
changing the application's public API or extracting a library. The benchmark
covers `semantic_tokens`, `completion_items`, and `identifier_ranges`; it does
not measure the complete LSP process, request handling, or compiler execution.

The bench uses the fixed Bend fixture in `benches/fixtures/analyzer_input.bend`
and `std::hint::black_box` for inputs and results. It uses Iai-Callgrind's
`#[library_benchmark]`, `library_benchmark_group!`, and `main!` APIs.

## Crate, runner, and build setup

Iai-Callgrind's published crate is currently **0.16.1**; upstream renamed the project and packages to **Gungraun** starting at 0.17.0 ([crate release page](https://docs.rs/crate/iai-callgrind/latest), [upstream changelog](https://github.com/gungraun/gungraun/blob/main/CHANGELOG.md)). If retaining the `iai-callgrind` package/API, pin `iai-callgrind` and `iai-callgrind-runner` to the same version. The official install guide uses a dev dependency and requires `harness = false`; `main!` replaces the default harness, and the matching runner executable must be available on `PATH` (or `IAI_CALLGRIND_RUNNER`): [0.16.1 installation guide](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/installation/iai_callgrind.md) · [`main!` API](https://docs.rs/iai-callgrind/0.16.1/iai_callgrind/macro.main.html).

Iai-Callgrind requires debug symbols. The release profile strips symbols, so `Cargo.toml` sets `[profile.bench] debug = true` and `strip = false` to retain them ([manifest](../Cargo.toml#L63-L71), [prerequisites](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/installation/prerequisites.md)).

## Regression limits and baselines

Callgrind records `Ir` (instructions executed). Available cache events include `I1mr` (L1 instruction-cache read misses) and `ILmr` (last-level instruction-cache instruction misses); the `EventKind` reference defines cache events as requiring cache simulation (`--cache-sim=yes`) ([EventKind 0.16.1](https://docs.rs/iai-callgrind/0.16.1/iai_callgrind/enum.EventKind.html)). The documented default Callgrind metrics include cache-hit metrics, but for explicit instruction-cache miss limits configure cache simulation explicitly.

The `performance` workflow enforces percentage soft limits of 2% for `Ir` (instructions) and 3% each for `I1mr` and `ILmr` (instruction-cache misses). These are policy ceilings, not measured results; change them only through maintainer-reviewed policy updates.

```sh
cargo bench --bench analysis -- \
  --baseline=main \
  --callgrind-args='--cache-sim=yes' \
  --callgrind-limits='ir=2%,i1mr=3%,ilmr=3%'
```

The workflow passes these limits at invocation, overriding any benchmark-file limits. An over-limit regression fails the benchmark with exit code 3. This gate measures instruction counts and instruction-cache misses; it does not claim cycle or allocation measurements. For Cachegrind as the selected tool, the separate options are `--cachegrind-limits` / `IAI_CALLGRIND_CACHEGRIND_LIMITS`. [Upstream regression guide (v0.16.1)](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/regressions.md) · [CLI reference (v0.16.1)](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/cli_and_env/basics.md)

By default, consecutive runs are compared. To compare a candidate to a stable named reference, first save it on the reference revision, then make that named baseline available to the candidate run:

```sh
# On the reference revision
cargo bench --bench analysis -- --save-baseline=main

# On the candidate revision, with the saved target/iai data available
cargo bench --bench analysis -- --baseline=main \
  --callgrind-args='--cache-sim=yes' \
  --callgrind-limits='ir=2%,i1mr=3%,ilmr=3%'
```

`--save-baseline=NAME` compares to an existing named baseline if present and then replaces it; `--baseline=NAME` compares without replacing. Baselines are benchmark output data (by default under `target/iai`), so CI must preserve/pass that data or generate it from the chosen reference revision. Merely requesting a baseline comparison is distinct from setting a threshold; limits make an over-limit regression fail. [Baseline guide (v0.16.1)](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/cli_and_env/baselines.md) · [CLI reference (v0.16.1)](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/cli_and_env/basics.md)

## CI and local platform constraints

This is not intrinsically Linux-only: Iai-Callgrind requires Valgrind and therefore only runs on a platform Valgrind supports. Upstream's CI installation examples use the matching runner version, and its prerequisites list Linux distributions and FreeBSD for Valgrind installation ([prerequisites](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/installation/prerequisites.md), [runner install/CI guidance](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/installation/iai_callgrind.md)). A Linux CI job with Valgrind installed is the narrow, conventional gate; `apt-get install valgrind` is the upstream Debian/Ubuntu example.

The native macOS/arm64 host is not a supported Valgrind platform. The persistent `rust` service in [`compose.yaml`](../compose.yaml) runs Linux/ARM64 with the pinned toolchain, Valgrind, Iai runner, and named Cargo/build-cache volumes, so the benchmark can run in Docker without treating macOS itself as supported ([Valgrind supported platforms](https://valgrind.org/info/platforms.html)).

The committed [`performance` workflow](../.github/workflows/performance.yml) saves a `main` baseline from the pull request's base SHA in ignored `target/iai`, then compares the pull-request merge revision with the stated limits. It runs only for pull requests targeting `main` and measures the three analyzer calls in `benches/analysis.rs`, not end-to-end LSP latency.

A local Linux/ARM64 Docker run completed all three benchmarks; after restarting the persistent service, Cargo reused the compiled bench artifact. This was a harness smoke test without a saved `main` baseline, not a regression comparison. The PR workflow is the gate that compares against the base SHA and applies limits. GitHub branch protection is server-side and remains unconfigured because this local checkout has no remote.