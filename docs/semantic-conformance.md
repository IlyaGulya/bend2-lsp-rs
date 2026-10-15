# Semantic conformance

## What the oracle proves

The production server does not invoke the oracle. `examples/semantic_probe.rs`
projects the same immutable `SemanticView` used by LSP into a test-only JSONL
protocol. The differential worker executes the vendored Bend compiler, not an
independent implementation of name resolution.

The compiler and Base are exact official Bend 2.0.37 source bytes from commit
`4ff8545f19b09b7d2b8ca2303668100eac8efd66`. Installation verifies their sizes and
SHA256 hashes against both the semantic fixture pin and the production
`scripts/bend_compiler_pins.json`; release identity and source archive provenance
must agree. The fixture includes frozen compiler/Base bytes and the license,
without a functional compiler fork or derivative patch.

The observer instruments those frozen bytes separately to report declarations,
binding origins, references, and effective signature/body ownership.
Instrumented and pristine acceptance and complete ordered Book contents are
checked for parity. This establishes source-byte conformance under the pinned
test runtime, not executable/runtime equivalence to distributed Bend binaries
or compatibility with Bend 2.0.36 or future compiler versions.

The Bun runtime is pinned to 1.4.0 and full revision
`34cbb9a40b4bd1bd767d134a7065e66c2432a676`, matching the production compiler pin.
Installation authenticates platform archive and executable bytes, version, and
full revision; an explicitly supplied executable cannot attest its own identity.
The worker canonicalizes its sandbox before enforcing path containment. Every
case gets a fresh compiler Book; A/B/A, repeated, reversed-order, and fresh-process
checks detect state leakage.

One project remains in flight. Its instrumented and pristine native compiler
paths run concurrently in isolated Bun threads, with identical persistent
request histories and a fresh Book in each path. Complete ordered native
`term_key(term_lower(...))` components are compared through a credited, bounded
stream; Base is neither cached nor omitted from parity. Per-path stage durations
overlap and must not be summed as wall latency. Required bounded setup also runs
native Book-field mutation, stream corruption, worker-failure, and EOF contracts.

Exact source origins distinguish identical files and same-spelling declarations.
Comparisons include missing and extra edges, multiplicity, binding versus global
targets, roles, enclosing definitions, revisions, and signature/body ownership.
Selected real stdio requests additionally exercise production consumers.
Compiler-rejected and unfinished inputs use separate expectations: they do not
count as valid differential comparisons or as skipped successful cases.

This is not an oracle for the production multi-error recovery patches. That
separate task, `bend2-lsp-rs-rbt.54`, must compare production-patched and pristine
compiler diagnostics along real repair chains: after repairing error A, the
pristine compiler must expose the recovered error B with the correct origin.

## Local commands

From the repository root:

```sh
python3 scripts/semantic_install.py --download-runtime --report target/semantic-install.json
cargo build --release --locked --bin bend2-lsp --example semantic_probe
python3 scripts/semantic_conformance.py startup \
  --candidate target/release/examples/semantic_probe \
  --report target/semantic-startup.json
python3 scripts/semantic_conformance.py plan --profile corpus \
  --output target/semantic-corpus-plan.json
python3 scripts/semantic_conformance.py run \
  --plan target/semantic-corpus-plan.json \
  --candidate target/release/examples/semantic_probe \
  --lsp target/release/bend2-lsp \
  --report target/semantic-corpus-report.json \
  --artifact-dir target/semantic-corpus-failures
```

Reinstall the observer after changing its pinned worker or compiler inputs.
`--profile pr` creates the permanent corpus plus exactly 256 generated projects
with seed 5301, including layout variants. Generation is versioned and bounded
(four files, 16 KiB per file, 64 KiB total, maximum depth eight). The gate requires
unique input and normalized semantic structures; merely renaming one template
256 times is not sufficient. Required corpus cases cannot be disabled with an
environment variable.

The required quality entry point is `./scripts/quality`. It runs supervisor
contract tests, existing Rust/security/dependency checks, separately measured
semantic setup, and one bounded conformance run. The required gate uses the
production release profile (optimization level 3, fat LTO, one codegen unit,
stripped symbols), not debug binaries. Individual supervisor commands:

```sh
python3 scripts/semantic_gate.py install
python3 scripts/semantic_gate.py setup
python3 scripts/semantic_gate.py run
```

## Failure handling

Each supervisor attempt gets its own directory under `target/semantic-gate`.
Reports preserve plans, provenance, completed counts, raw mismatches, stderr,
and replay bundles, including unsuccessful attempts. A timeout, worker failure,
missing comparison, stale pin, extra output, or resource-limit violation fails
closed. A passing count cannot hide an omitted planned case.

```sh
python3 scripts/semantic_replay.py replay PATH_TO_BUNDLE --recorded-tools
python3 scripts/semantic_replay.py replay PATH_TO_BUNDLE --shrink pr --recorded-tools
python3 scripts/semantic_replay.py promote PATH_TO_BUNDLE --output PATH_TO_NEW_CASE
```

Replay authenticates recorded tools and reproduces the actual mismatch.
Shrinking keeps the original bundle and accepts a reduction only when the same
failure predicate survives; a same-spelling replacement binder is not the same
local-binding bug. Review and add promoted cases to the permanent corpus rather
than blessing candidate output as compiler truth.

## Resource and performance contracts

The initial PR profile is **provisional**, not calibrated evidence:

| Constraint | Initial profile |
| --- | ---: |
| Generated projects | 256 |
| Worker concurrency | 1 |
| Successful-run p95 target | 60 s |
| Hard measured-run deadline | 120 s |
| Setup target, measured separately | 300 s |
| Aggregate process-tree RSS ceiling | 1 GiB |
| RSS sample interval | 20 ms |
| Attempt artifact bytes | 32 MiB |
| Attempt artifact files | 512 |

The supervisor samples aggregate descendant RSS using Linux `/proc` or macOS
`ps`. This is not a hard cgroup memory limit and can miss peaks between samples.
The interval is a polling sleep plus scan overhead; reports include
`max_rss_sample_seconds` so delayed observations remain visible. macOS scans
wait interruptibly within the active phase deadline, not an unrelated fixed
subprocess timeout. A stalled observer cannot turn an expired phase into success.
The supervisor also bounds output/report sizes and kills/reaps process groups,
including children whose group leader has already exited. Reports distinguish
setup, startup, comparison, normalization, stdio, and shutdown costs.

The independent Linux Callgrind gate still measures cold snapshots separately
from warm analysis. Existing Ir, I1mr, ILmr metrics, thresholds, baseline
selection, and missing-row policy are unchanged. New workloads cover metadata,
entry views, and indexed warm queries. Local timing or an ARM smoke run is not
permission to accept a Callgrind regression. See [performance policy](performance-policy.md).

## CI calibration and longer search

`.github/workflows/semantic-fuzz.yml` is unprivileged and separate from release
publication. Scheduled search has a 45-minute search phase, up to ten minutes of
shrinking, and a 60-minute hard supervisor deadline; preparation and upload are
outside that deadline. Original failure bundles are retained.

A manual dispatch on `main` starts five independent calibration jobs. Before
merge, maintainers can opt a PR targeting `main` into the same calibration by
adding the `semantic-calibration` label. Synchronization and reopening rerun it
while labeled. All five VMs and aggregation check out the same immutable PR merge
SHA; unrelated labeling does not start calibration. Scheduled long search stays
main-only and unprivileged.

Each job collects four sequential complete runs, cold/warm setup, startup-only,
and fixed 128/256/512-project scaling evidence. Scaling currently retains the
same hard 120-second ceiling; a larger workload receives no automatic allowance.
The aggregate report requires at least 20 runs from five jobs, with at least four
distinct sequential measurements per job. Gate-generated measurement IDs and
per-job run ordinals reject copied reports and skewed allocations such as
16 + 1 + 1 + 1 + 1. Source, candidate, profile, and provenance must match;
missing or failed supporting phases reject admission. The report computes the
observed p95; it does not automatically change policy.

Do not label the profile calibrated until that real CI evidence exists and is
accepted. Updating a compiler pin requires reviewing the source/instrumentation
diff, rerunning the complete corpus and generated comparisons, inspecting changed
semantics, and recalibrating affected costs. Do not update checksums simply to
make a disagreement disappear. Additional broad discovery or global-index
expansion requires its own workspace measurements and admission; conformance
does not authorize it or replace the existing discovery service.
