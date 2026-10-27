# Semantic conformance

## What the oracle proves

The production server does not invoke the oracle. `tools/semantic-probe/src/main.rs`
projects the same immutable `SemanticView` used by LSP into a test-only JSONL
protocol. The differential worker executes the vendored Bend compiler, not an
independent implementation of name resolution.

The projection is an isolated, unpublished workspace binary rather than a Cargo
example. Required release preparation builds only the server and this tool;
the server's benchmark-only dev-dependencies are not part of that build.
It inherits the workspace's Rust version and lint policy and keeps the same
release optimization settings and JSONL protocol.

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

The adapter avoids allocating Base-local term, binding, call, retain-use and
type-origin observations that extraction previously discarded.
Extra observer-only binding spans are skipped for those discarded observations.
Base file, owner, declaration and implementation-origin events remain available, including
`inspect_base`. Base parsing, checking and complete canonical comparison are
unchanged; no Book or Base result is cached.

The Bun runtime is pinned to 1.4.0 and full revision
`34cbb9a40b4bd1bd767d134a7065e66c2432a676`, matching the production compiler pin.
Installation authenticates platform archive and executable bytes, version, and
full revision; an explicitly supplied executable cannot attest its own identity.
The worker canonicalizes its sandbox before enforcing path containment. Every
case gets a fresh compiler Book; A/B/A, repeated, reversed-order, and fresh-process
checks detect state leakage.

Two independent persistent oracle sessions overlap the complete forward
differential history and complete reversed valid history. Each session has at
most one project in flight. Within each session, instrumented and pristine native
compiler paths run concurrently in separate authenticated module contexts: the
owner and its pristine worker thread follow the same ordered request history,
with a fresh Book in each path. The worker uses bounded credited messages,
rejects stdout noise, and exits before its owner completes EOF, SIGTERM or SIGINT
shutdown. Worker threads retain the established runtime and GC configuration.
No mutable Book or Base is shared across requests or sessions. Complete ordered native
`term_key(term_lower(...))` components are compared through a credited, bounded
stream; Base is neither cached nor omitted from parity. Per-path stage durations
overlap and must not be summed as wall latency. Required bounded setup also runs
native Book-field mutation, stream corruption, worker-failure, and EOF contracts.

The forward controller sends its native request before querying the Rust
candidate, so the candidate can run while the oracle is busy. This overlap adds
no further processes or threads. Each RPC retains its absolute reply-arrival deadline;
later consumption does not extend that deadline. A completed native response is
preserved before a deferred candidate error is raised. Forward oracle and
candidate RPC durations can overlap and must not be summed as wall latency.

Both native VMs use ordinary Bun heaps. At idle Book boundaries, synchronous
[young-generation collection](https://bun.com/reference/bun/jsc/edenGC) reclaims
short-lived allocations. If the reported heap after that collection exceeds
32 MiB, [full collection and sweeping](https://bun.com/reference/bun/jsc/gcAndSweep)
reclaims older unreachable objects before continuing. This is a soft collection
trigger, not a memory allowance or a guarantee that RSS is below its ceiling.
Neither VM is recreated and both retain their complete ordered histories.

Credited canonical batches keep their 65,536-UTF-16-code-unit payload ceiling.
Up to 256 small chunks share a batch to reduce round trips; individual chunk
bounds, exact component comparison, and credit/sequence checks are unchanged.
Comparison checks all four chunk fields directly: component, UTF-16 offset,
complete string contents, and the final-chunk flag. No per-chunk callback or
temporary declaration-kind list is needed. Batch validation still precedes
comparison, and truncation/reordering/mutation checks retain their error
precedence. This changes neither the canonical bytes nor the batching bounds,
official lowering/keys, Base coverage, histories, GC settings or admission limits.
Runtime archive/member authentication uses bounded streams over a private
authenticated snapshot.
Full native replies remain in the visible, artifact-budgeted compressed spool;
later consumers load authenticated replies instead of retaining every decoded
Base declaration table in RAM.

The forward session completes all 634 planned views (627 valid comparisons,
five intended-invalid views, two recovery views), then performs the proof-context
A/B/A and repeated-case checks in that same session. The reverse session covers
all 627 valid views in exact reversed order and retains bounded IDs and semantic
hashes until every result is compared with the forward history. It is joined
and shut down before the fresh-worker comparison and 13 real stdio projects
(418 consumer checks). A timeout, EOF, mismatch, or early forward failure cancels
and joins the reverse owner; original failures remain failures, not partial
successes. Reports include independent worker PIDs, complete reverse IDs/hashes,
per-history start/finish/close times, per-case RPC intervals, and the measured
intersection of forward/reverse RPC intervals (`oracle_rpc_overlap_seconds`).

Exact forward/reverse native reply bytes are temporarily retained in one visible,
compressed spool under the attempt's artifact directory. The spool is charged
against the existing artifact byte/file ceilings, checks those ceilings before
each append, retains only bounded indexes in memory, and is deleted once history
comparison succeeds or during failure cleanup. A state-leak bundle contains both
original replies, their IDs/hashes, ordered prior history identities, worker PIDs,
and pinned provenance. Cancellation and owner joining share the pinned two-second
shutdown grace and remaining deadline; repeated cleanup does not renew that grace.

Reverse-history integrity failures use an explicit `oracle-history` bundle:
both complete ordered native request prefixes, both original target replies,
pins, and prior-history identities are recorded. Replay reconstructs two native
sessions and repeats them in fresh processes; it never substitutes the Rust
probe for a native history. Bounded history-prefix shrinking retains the target
and the same observed divergence. These failures diagnose an untrustworthy
oracle and cannot be promoted as production semantic truth. Completed reverse
divergences are preserved alongside a later forward failure before spool cleanup.

Exact source origins distinguish identical files and same-spelling declarations.
Comparisons include missing and extra edges, multiplicity, binding versus global
targets, roles, enclosing definitions, revisions, and signature/body ownership.
Selected real stdio requests additionally exercise production consumers.
Compiler-rejected and unfinished inputs use separate expectations: they do not
count as valid differential comparisons or as skipped successful cases.

Stdio range conversion uses LF-only line boundaries, matching compiler sites
and `LineIndex`, rather than Python's broader Unicode line splitting.
Vertical tab, form feed, NEL and Unicode line/paragraph separators stay within
their source line. Columns still count UTF-16 code units and locations retain
UTF-8 byte offsets, including non-BMP characters.

This is not an oracle for the production multi-error recovery patches. That
separate task, `bend2-lsp-rs-rbt.54`, must compare production-patched and pristine
compiler diagnostics along real repair chains: after repairing error A, the
pristine compiler must expose the recovered error B with the correct origin.

## Local commands

From the repository root:

```sh
python3 scripts/semantic_install.py --download-runtime --report target/semantic-install.json
cargo build --release --locked --package bend2-lsp --package bend2-semantic-probe --bins
python3 scripts/semantic_conformance.py startup \
  --candidate target/release/semantic_probe \
  --report target/semantic-startup.json
python3 scripts/semantic_conformance.py plan --profile corpus \
  --output target/semantic-corpus-plan.json
python3 scripts/semantic_conformance.py run \
  --plan target/semantic-corpus-plan.json \
  --candidate target/release/semantic_probe \
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

On macOS, first build the source-matched native RSS observer with
`cargo build --locked --package bend2-perf --bin bend2-perf`.
The quality entry point builds it before the supervisor contracts. Standalone
commands use `CARGO_TARGET_DIR/debug/bend2-perf` (or `target/debug/bend2-perf`);
`BEND2_SEMANTIC_OBSERVER` selects an explicitly provisioned observer instead.
This builds monitoring tooling, not the separately timed release candidate.

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

The PR profile `semantic-pr-v8-full-reference-budget` is **provisional**,
not calibrated evidence. Its two case-worker pairs use bounded reply retention
and ordinary native heaps with idle collection. Relative to the earlier
provisional profile, only the measured-run admission budget changed: the
successful-run p95 target is 120 s and the hard measured-run deadline is 240 s,
which also bounds every 128/256/512-project scaling measurement. Corpus, seed,
release settings, and all other limits remain unchanged:

| Constraint | Initial profile |
| --- | ---: |
| Generated projects | 256 |
| Worker concurrency (independent pristine/instrumented pairs) | 2 |
| Successful-run p95 target | 120 s |
| Hard measured-run deadline, including 128/256/512 scaling | 240 s |
| Setup target, measured separately | 300 s |
| Aggregate process-tree RSS ceiling | 1 GiB |
| RSS sample interval | 20 ms |
| Attempt artifact bytes | 32 MiB |
| Attempt artifact files | 512 |

The supervisor samples aggregate descendant RSS using Linux `/proc` or a
persistent macOS native observer. This is not a hard cgroup memory limit and
can miss peaks between samples. The interval is a polling sleep plus observation
overhead; reports include `max_rss_sample_seconds` so delays remain visible.
The native observer reads current resident memory, not peak or cached exit RSS.
An unavailable task is treated as exited only with a stable kernel identity and
positive zombie/exit-state evidence; live or inaccessible observations fail.
Its own RSS and child CPU are counted, without adopting it as a workload child.
Bounded IPC, lazy startup, termination and reaping consume the existing phase
deadline. A failed work observer is terminal: work is never resumed or retried.
After closing and reaping that instance, cleanup may start one separate
shutdown-only verifier within the remaining original shutdown reserve. Its
resources are also counted. Reports preserve the original work/shutdown
observer errors; successful cleanup cannot turn failed work into success.
Helper reaping uses a bounded subprocess wait rather than sleeping for a full
RSS sampling interval after each kill. The wait uses only the remaining original
deadline; an expired deadline or an unreaped helper still fails cleanup.
The supervisor also bounds output/report sizes and kills/reaps process groups,
including children whose group leader has already exited. Reports distinguish
setup, startup, comparison, normalization, stdio, and shutdown costs.
A complete observation with no live owned child may be reused only after the
worker has exited successfully and stdout reached EOF. Cleanup still signals
owned groups, waits the leader and reaps adopted Linux children. A final
nonblocking leader poll and adopted-child drain precede CPU accounting, including
children that became zombies during the last quiet scan. These add no wait
allowance and cannot erase an earlier cleanup failure. Live children or
unavailable monitoring still require fresh cleanup observations. CPU reported
after forced termination is not proof of a complete tree-CPU measurement.
On macOS, reaping a group leader can precede reaping its orphaned descendants.
An EPERM signal therefore waits only within the existing 100ms signaling grace
for successful delivery or kernel-reported group absence. Permission errors for
surviving inaccessible groups still fail; the cleanup deadline is not extended.

The independent Linux Callgrind gate still measures cold snapshots separately
from warm analysis. Existing Ir, I1mr, ILmr metrics, thresholds, baseline
selection, and missing-row policy are unchanged. New workloads cover metadata,
entry views, and indexed warm queries. Local timing or an ARM smoke run is not
permission to accept a Callgrind regression. See [performance policy](performance-policy.md).

## Diagnostic CPU profiles

The branch-scoped `semantic-oracle-profile.yml` workflow first runs the unchanged
`./scripts/quality`, then captures the instrumented and pristine VMs through
`scripts/diagnostics/oracle_cpu.py`. Profiling is hosted-only. The capture keeps
its own explicitly declared 240-second diagnostic deadline for profiler
overhead; that value currently coincides with the v8 measured-run hard deadline,
but it remains a diagnostic-only declaration and never supplies admission
evidence or relaxes the normal resource, p95 or Callgrind contracts above.

`capture-manifest.json` records both `original_resource_limits` and
`diagnostic_resource_limits`, which are identical under the v8 budget, and always
declares `performance_admission: false`.
Memory, output, artifact and shutdown limits remain unchanged. The earlier v7
120-to-240-second diagnostic allowance is historical, not a rescore of its failures.
Complete corpus, both histories, Base/pristine parity,
fresh/ABA/repeated isolation and real stdio must match the authenticated reference.
Each owner records its sampling policy and explicit pauses. Sparse samples and
instrumented elapsed time are not continuous coverage or calibration evidence.
Missing owners, final flushes or semantic observations reject a complete capture.

Cross-run semantic fingerprints cover valid answers. Rejected raw replies also
contain per-run timing metrics, so their raw hashes are evidence, not stable
semantic identities. The unchanged full harness still checks every intended
rejection and recovery assertion; diagnostic reference checks additionally
require identical case identities, semantic/rejection coverage, histories and
stdio observations.

## Canonical-key representation qualification

The oracle still calls the official `term_lower` for every type, body and
constructor in each fresh Book, including Base. `canonicalTermKey` projects
source fields with copy-on-write and uses native JSON serialization without a
per-property replacer. Only paths needing exclusion are copied; already-clean
objects are reused read-only. It preserves every other own field and property
order, including fields unknown to a term-tag whitelist. It never edits shared
leaves or source annotations and does not cache keys across Books.
The projection is defined for pinned plain native term data, not arbitrary
JavaScript objects with getters or custom `toJSON` hooks.

`test_semantic_book_stream.ts` compares exact bytes with the official `term_key`
over native constructors, generated terms and Base-loaded Books, and checks source
and alias preservation. Additional `scripts/diagnostics/canonical_key.py`
qualification checks every current PR-corpus view against both the plain and
instrumented compiler modules. It preserves intended rejection observations and
compares all module observations, not hashes containing runtime timings.

```sh
python3 scripts/semantic_install.py --download-runtime --native-contracts
python3 scripts/diagnostics/canonical_key.py --correctness-only \
  --output target/canonical-key-local
```

The branch-scoped `semantic-canonical-key.yml` workflow additionally records
alternating native/candidate canonicalization costs on hosted Linux. Each arm
includes official lowering and the existing idle-heap policy. These are
component-level observations, not a new admission threshold, full-runtime p95
qualification or permission to change the original quality/Callgrind gates.
Local performance measurement remains forbidden.

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
and fixed 128/256/512-project scaling evidence. Scaling retains the same hard
240-second ceiling for each measurement; a larger workload receives no automatic
allowance and no phase is skipped.
The aggregate report requires at least 20 runs from five jobs, with at least four
distinct sequential measurements per job. Gate-generated measurement IDs and
per-job run ordinals reject copied reports and skewed allocations such as
16 + 1 + 1 + 1 + 1. Source, candidate, profile, and provenance must match;
missing or failed supporting phases reject admission. The report computes the
observed p95; it does not automatically change policy.

The `6ee0f23` [CI cohort](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/39208885161)
completed all 20 normal runs with 627 exact semantic comparisons each. Its
descriptive p95 was 114.771427 s, not an admitted result against the historical
60 s target. Four supporting jobs timed out at 512 projects; one timed out at 256
and did not attempt 512. The official aggregate rejected the incomplete
supporting evidence. That cohort remains historical v7 evidence measured under
its then-current 120 s hard and 60 s p95 limits; it is not rescored under the v8
budget, and every failed job remains failed.

Do not label the profile calibrated until that real CI evidence exists and is
accepted. Updating a compiler pin requires reviewing the source/instrumentation
diff, rerunning the complete corpus and generated comparisons, inspecting changed
semantics, and recalibrating affected costs. Do not update checksums simply to
make a disagreement disappear. Additional broad discovery or global-index
expansion requires its own workspace measurements and admission; conformance
does not authorize it or replace the existing discovery service.
