# Performance regression policy

## Scope

The package exposes the `bend2_lsp` library for core analysis and workspace
indexes; the `bend2-lsp` binary keeps the LSP/server adapter private. The
benchmark calls these production library APIs directly. The group
covers cold snapshot construction for small (2,552-byte), medium (32,429-byte),
and large (260,429-byte) sources; warm semantic-token, completion, identifier,
reference, call-hierarchy, inlay-hint, and indexed parameter-annotation queries;
warm indexed constructor-definition queries; ASCII/Unicode position
conversion; folding on 100-, 1,000-, and 10,000-line inputs; and a generated 100-file
workspace with 601 declarations and 4,758 call sites. Workspace measurements
include initial graph loading, cross-file references, one dependency revision,
a 16-revision burst, and workspace-symbol response materialization for empty and
mixed-case queries. Warm snapshots and loaded workspace fixtures are
constructed in Iai benchmark argument setup, outside the measured query; cold
snapshot benchmarks include construction.
The owned `LoadedWorkspace` arguments to `workspace_references` and
`workspace_symbols_warm` are destroyed inside their measured functions,
including database snapshots and temporary source trees. Their totals therefore
include teardown and are not query-only instruction counts. The workspace-symbol
cases retain owned names, kinds, ranges and document identities in one response;
they cover analysis materialization, not protocol serialization or editor latency.
Symbol queries normalize once across indexed documents and project borrowed
names into the final owned response. Empty queries skip name case folding;
the LSP handler reserves indexed declaration capacity for its final response.
The direct-analysis benchmark retains its original response ownership and
workload, so it does not measure the handler's URI cloning or reservation.
The separate `workspace_symbol_response_warm.ten_thousand_symbols` case consumes
an owned, preconstructed 10,000-symbol protocol result with the discovery
workload's function names and ranges. It calls the pinned framework's actual
`IntoResponse` conversion and encodes the complete JSON-RPC response. Fixture
construction is outside measurement; result conversion, wire encoding and owned
input/intermediate teardown are included. This covers the previously unmeasured
response serialization path, not discovery, the full LSP process or editor latency.
The fixed URI root and workload remain identical across baseline and candidate.

The `reference_location_order_warm` cases cover stable ordering and deduplication
of 64, 1,024, and 10,000 owned reference locations. The server and benchmark share
the same private `reference_locations` source module; no server internals are
exported for measurement. URI/range construction and deterministic input
shuffling occur in argument setup. Ordering, deduplication and owned-input
teardown are measured; workspace lookup, discovery, wire serialization and the
full LSP process are not. Unchanged-algorithm measurements were collected before
the borrowed-URI correction. Those archived measurements support an experimental
before/after comparison, not admission of a later candidate.
The ordering cases run in their own `reference_locations` benchmark binary;
the `analysis` binary retains its existing workloads. Current main contains both
targets. CI builds both original source revisions independently and retains
both executable inventories, hashes and isolated raw profiles. Each of the
105 source pairs uses fresh processes and output directories; it does not reuse
a saved `main` counter snapshot. Every baseline workload is required.
Candidate-only workloads remain explicitly unpaired until a later baseline
contains them.

Indexed query result capacity must not introduce a second warm semantic pass.
The scanner counts semantic-token-producing token kinds as it emits tokens.
Call construction counts arguments eligible for local function parameter hints.
These two scalar counts let semantic-token and whole-document hint queries
reserve their result capacity without recounting indexed tokens or calls.
Bounded hint ranges retain local filtering and do not reserve whole-document
hint capacity. Cold snapshot construction remains separately gated.
Scoped completion deduplicates matching indexed `NameId` values in a sorted
temporary vector; it preserves alphabetical local results, declaration order,
shadowing and keyword suppression. Import candidates compare `FileId` values
and reuse the already selected effective snapshot. All existing workload
bodies, result ownership, cache geometry and numeric limits remain unchanged.

The `semantic_view_cold.declarations_{100,1000,10000}` cases construct entry
views from captured snapshot facts. Declaration-name occurrences reuse existing
snapshot-local IDs with an exact range check. Construction caches each file's
generation/conflict facts and each entity's signature template arity; it borrows
shared snapshots and preallocates qualified names rather than repeatedly
searching declarations, hashing paths or cloning document metadata.

A local Linux ARM64 diagnostic against `19ec528`, using pinned Valgrind 3.27.1
and the unchanged 32 KiB I1/D1 and 8 MiB LL geometry, exercised all 96 analysis
workloads once per revision with positive instruction counters throughout:

| Cold-view declarations | Before instructions | After instructions |
| --- | ---: | ---: |
| 100 | 2,752,351 | 1,832,008 |
| 1,000 | 30,952,226 | 19,115,829 |
| 10,000 | 574,473,391 | 186,083,914 |

This is diagnostic evidence, not admission: it is not the 105-pair hosted
comparison, does not include the separate reference-order binary, and does not
establish editor latency or native-oracle resource qualification. Four workloads
had instruction increases above 5% in this single local comparison; they are not
waived or hidden by the lower aggregate. Existing CI gates remain authoritative
for every instruction and independent cache metric.

The subsequent hosted comparison of `6ee0f23` (tested merge `ca105e33`) completed
all 105 fresh pairs in run
[39208885069](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/39208885069).
Collection succeeded within the unchanged 210-minute measurement deadline, but
numeric admission did not: 22 of the 54 baseline workloads passed and 32 failed.
The exact-run exception was rejected; this comparison remains blocking. In
particular, larger standalone reference records introduced per-record copy
calls; instruction-cache regressions also remain independently blocking.

Reference queries now project borrowed compact headers while semantic linking
retains complete indexed records in the same arena. A throwaway program against
actual Rust 1.98.1 ARM64 release metadata observed:

| Representation | Copied reference bytes | Retained full-record bytes |
| --- | ---: | ---: |
| Expanded record in `6ee0f23` | 160 | 160 |
| Compact projection | 104 | 168 |

The extra full-record padding is an explicit retention trade-off, not omitted
metadata or a memory-budget exception. Frozen `Vec` buckets also retain their
capacity metadata instead of slice-only metadata; avoiding table reconstruction
does not waive retained-memory qualification. The projection preserves target grouping
and order; full source facts are still prepared cold. Final semantic-view bucket
compaction reuses existing hash tables, cold grouping reuses span cursors, and
ASCII workspace-symbol matching avoids allocating lowercase names while keeping
the Unicode path. All workload bodies, ownership/teardown boundaries and gates
remain unchanged. Correctness smoke and layout sizes are not hosted performance
admission.


A paired candidate with zero collected instructions against a nonzero baseline
is a collection failure, not a performance improvement. The authoritative
summary reader rejects it before producing or validating an admission verdict.
Zero cache-miss counters and zero-baseline arithmetic remain valid; this does
not change any numeric allowance. Run 38555552166 exposed an empty analysis
collection and must not be cited as a passing performance comparison.

The Linux CI Callgrind collectors pin upstream Valgrind 3.27.1 through
`./scripts/install-tools.sh valgrind`, with `VALGRIND_INSTALL_ROOT` set to an
absolute, dedicated directory outside the checkout. The official source archive
checksum and installed executable hashes bind the native-tool cache to its
provenance.
Collectors validate the actual version and retain this provenance beside raw
evidence. The absolute installation survives the baseline source checkout;
baseline and candidate are freshly collected with the same pinned profiler.

The maintainer approved this profiler cutover after the
[same-executable control](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38563507337)
showed that the old Ubuntu Valgrind 3.22.0 failed to register the revised
analysis executable, while upstream 3.27.1 restored its original guard and
production-function collection. This control is not a performance comparison.
Historical counts from the old profiler do not supply the new admission
baseline. The cutover changes no workload, guard boundary, cache geometry,
comparison metric, numeric allowance or exception requirement.

The first fresh symmetric
[comparison after the cutover](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38567984469)
used main `8010fe8`, candidate head `156fb77` and exact merge `7cacbd3`.
All 51 paired workloads and all three new reference-ordering workloads collected
positive instructions. Both roles retained the declared 32 KiB, eight-way I1/D1
and 8 MiB, 16-way last-level geometry with 64-byte lines.
The numeric verdict was still a regression: 45 of 51 paired workloads passed.
The remaining failures were all three `completion_warm` sizes,
`workspace_symbols_warm` empty/case-insensitive queries and
`workspace_burst_revision_invalidation`. A successful collection job does not
turn that verdict into a passing performance comparison or an approved exception.

The next [comparison](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38576255963)
used candidate head `bcf37db` and exact merge `10e77bc` against the same main.
All 51 paired workloads collected positive instructions; 48 passed the unchanged
numeric policy. All six failures from the preceding comparison passed.
The remaining regressions were I1 misses in `identifier_ranges_warm.large`
and last-level misses in the exact and fuzzy `import_completion_candidates_warm`
queries. This remains a rejected performance comparison, not an exception.

The [scalar-ID and ASCII-matcher comparison](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38587146675)
used candidate head `8e3b4b0` and exact merge `6a96628`. It passed 45 of 51 paired
workloads, not 48. Exact/fuzzy import completion instructions fell to 161,800
and 164,942 from 198,552 and 201,818, but last-level misses still failed.
Both large and medium reference-range I1 counts failed, as did burst-invalidation
and empty workspace-symbol last-level counts. Lower instructions do not override
the independent miss limits; this comparison was not admitted.

The [dense declaration-name comparison](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38591157726)
used candidate head `0fa3819`. All 51 paired workloads collected positive
instructions; 46 passed. Large/medium reference queries passed at 605 instructions
versus 782 and eight last-level instruction misses versus ten. Cold large snapshot
construction, burst invalidation and both workspace-symbol queries also passed.
The remaining failures were the three unscoped completion sizes and exact/fuzzy
import completion. The whole comparison still required rejection.

The [declaration-only collector comparison](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38595160251)
used candidate head `5cc1fb6`. All 51 instruction pairs remained positive; 47
workloads passed. Sharing the iterator-based module collector did not improve
unscoped completion: all three sizes failed instructions, I1 and last-level limits.
Fuzzy import completion also failed its last-level limit. The collector must be
improved or the sharing cutover removed before this source can be admitted.

The [direct module-projection comparison](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38599671190)
used candidate head `24b25ad`. All 51 paired instruction counts remained positive;
49 workloads passed. Every import-completion case passed, as did small/medium
unscoped completion. The remaining failures were large completion I1 misses
(53 to 57) and small inlay-hint last-level instruction misses (77 to 82).
Both still fail the unchanged policy despite lower instruction counts.

The [materialized hint-cache comparison](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38603459702)
used candidate head `a0179f0` with 51 positive instruction pairs. Only 45 workloads
passed. All warm inlay-hint sizes passed; small hints changed from 19,181 to 744
instructions and from 77 to six last-level instruction misses. This is not an
admission win: cold snapshot last-level misses increased from 835 to 920 (small),
689 to 735 (medium), and 681 to 728 (large). Those three cold cases also failed
their I1 limits. Large/medium completion and burst invalidation failed I1 limits.
The full-record cache and repeated declaration traversal therefore require
replacement or removal rather than a cold-construction regression exception.

The [compact argument/parameter-link comparison](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38607977777)
used candidate head `fb83755`. All 51 paired instruction counts were positive;
46 workloads passed. Warm inlay instructions were 58,543 versus 502,449 (large),
20,186 versus 155,249 (medium), and 1,812 versus 19,181 (small), with every warm
inlay cache metric passing. Cold snapshot construction still failed I1/IL limits
in all three sizes. Small completion failed instructions (19,744 to 20,308),
and the workspace-symbol response failed I1 (135 to 142) despite identical
instruction counts. Lower warm cost does not authorize those regressions.

Raw self-cost attribution in that cold-small profile accounts for 52 new
last-level instruction misses in hint-specific stable-sort functions. The parsed
self-cost totals match the complete retained profile summary. This identifies
executed cold code, not a passing successor measurement.

The source-ordered call stream permits sequential hint appends. Nested arguments
can interleave with the pending argument suffix of enclosing calls; ordered
insertion moves that suffix without a second generic sort or scratch buffer.
Movement grows with nesting and enclosing-call arity. The unchanged cold-small
corpus covers mixed-arity interleavings; medium/large cover deep unary calls.
Cold and warm numeric gates still decide whether this tradeoff is acceptable.

The [source-ordered hint comparison](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38611258640)
used candidate head `e477708`. All 51 paired instruction counts were positive;
49 workloads passed. Cold-medium I1 misses were 801 versus 770; sixteen-revision
workspace invalidation had 12,540 versus 12,101 I1 misses. These two failures
still reject admission. All completion, warm hint and import workloads passed.

The next cold-construction change reuses the grouping count buffer for write
cursors instead of allocating and copying a second cursor vector. It also skips
hint construction when the exact eligible-argument count is zero. Neither
changes the persisted index representation or warm-query semantics; fresh
paired measurements must establish their cache effect.

The [reused grouping-cursor comparison](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38614088568)
used candidate head `1d3347e`. All 51 paired instruction counts were positive;
49 workloads passed. Cold-medium I1 misses decreased from 770 to 757, and
sixteen-revision invalidation I1 misses decreased from 12,130 to 11,516.
Admission still fails: large completion I1 misses were 57 versus 53, and
incremental invalidation instructions were 686,510 versus 656,992.

The next query changes seed unique direct reverse dependencies before allocating
cycle-tracking state, and queue only nonterminal entries. ASCII subsequence
matching prepares both case alternatives once per query byte instead of folding
each candidate-label byte. Consumer contracts cover terminal-to-transitive graph
updates, diamond deduplication, cycles, repeated letters, underscores and Unicode
fallback. A throwaway API smoke also checks every two-byte ASCII query against
the independent Unicode scorer and dependency graphs across forty revisions.
Only a fresh source-bound numeric comparison can establish admission.

`vendor/tower-lsp` is a maintained, Tokio-only fork of `tower-lsp` 0.20.0
at upstream commit `7b580128faa39b907db8f40bc53924181887acde`. The original
Apache-2.0 and MIT licenses and upstream revision metadata are retained.
Cargo's workspace patch selects this source for the server and response benchmark.
Generic successful results are serialized once into an owned JSON string;
the complete response encoder writes its bytes directly beside the encoded ID.
It does not construct or serialize an intermediate `serde_json::Value` tree.
Incoming messages retain the upstream serde-based decoding and response-first
dispatch. The fork does not enable `serde_json`'s `raw_value` feature.
The codec frames the complete encoded bytes with their actual `Content-Length`.
Workspace tests and feature checks include the fork; the quality gate also
checks its production library without panic-helper exemptions and runs its
documentation examples. Neither this design nor the isolated benchmark proves
editor latency or changes profiling deadlines, workloads or admission limits.

Integration tests compare exact semantic-token and identifier-range outputs with
legacy goldens at all three source sizes; medium and large goldens come from the
pre-index implementation in commit `bc8cd4f`. Completion is covered by behavioral
protocol cases for ranking, scope, type context and UTF-16 replacement boundaries,
not incidental label/detail/order goldens.

Cold completion-context construction first checks the snapshot's indexed keyword
names and skips documents without import/match/case contexts. Contexts remain
fully prepared before publication; warm queries never lazily scan source.
Completion filtering keeps the prefix fast path separate from ranking data.
It rejects impossible length matches and classifies each query once; ASCII
queries use byte comparisons without decoding candidate UTF-8. Non-ASCII queries
retain character matching, and ranking/UTF-16 edits are unchanged. The bounded
parameter-annotation lookup retains its inline query path.

Warm reference queries iterate the precomputed reference-index slice directly,
including an empty slice for an absent symbol or name. This preserves source
ordering and avoids an optional `flat_map` adapter in identifier/reference
collection; no semantic scan or secondary representation is built during a query.

Protocol stress tests also pipeline 32 hover requests during large document
edits/opens and print p50/p95/max response latency. Their `p95 < 1 second`
assertions catch prolonged blocking, not small latency regressions. The separate
paired latency reports below measure the actual LSP process, but not editor
rendering or real Bend compiler execution; real Bend CLI timings are measured
separately. Callgrind measures analysis functions and the isolated protocol-response
conversion above, not the full process.
The Callgrind measurement job preserves its raw baseline/candidate profiles,
summaries, baseline manifest, producer exit codes, and unchanged verdict for
14 days, including failed collection and numerical regressions. Collection
success and performance admission are separate checks; artifact retention and
human acceptance never rewrite the producer's outcome or historical red runs.

The bench uses the fixed Bend fixture in `benches/fixtures/analyzer_input.bend`
for analysis queries and generated workspace/folding fixtures for graph
workloads. Constructor-definition lookup uses a separate ADT snippet, with its
snapshot constructed before measurement. The bench uses Iai-Callgrind's `#[library_benchmark]`,
`library_benchmark_group!`, and `main!` APIs.

The `parameter_annotation_warm` cases cover builtin, qualified, and nested type
annotations. Setup builds the snapshot and selects the binding before measurement.
The query uses existing binding identities and delimiter contexts to inspect only
the indexed local type-token span; it does not reparse function headers or allocate
annotation tables during snapshot construction. Existing snapshot and binding
layouts, benchmark metrics, and regression limits remain unchanged.

## Ordinary measurement and exact-run admission

The [`performance` workflow](../.github/workflows/performance.yml) has separate
**Callgrind measurement** and **performance admission** checks. The active
`paired-median-105-v1` method uses 105 fixed, fresh-process pairs of the exact
base and merge-candidate sources. Both original benchmark harnesses are unchanged;
calibration-only helpers and controls are never inserted into ordinary builds.
Odd pairs execute A then B; even pairs execute B then A. Each source invocation
runs both benchmark executables with isolated output and a unique baseline name.
No adaptive stopping, outlier deletion, pooled hosts, retries for luck, or
numeric-limit changes are permitted.

The original `compare` job identity and `Compare pull request with main` step
remain in the source-bound verdict and artifact manifest. Collection must exit
0. The policy report exits 0 for a pass, 1 for confirmed regression, 3 for
inconclusive bounds, and 2 for invalid evidence. Actual outcomes and both exits
are retained even when the candidate step fails.

After `workflow finalize` inventories and hashes the inputs, `bend2-perf
admission measurement --measurement-method paired-median-105-v1` recomputes the
canonical verdict from retained evidence. It checks the exact base, candidate,
PR head and workflow SHAs, run ID and attempt against the caller's expectations.
Source manifests, executable identities, complete workload inventories, raw
stdout and Callgrind counters must agree. Raw profiles use the pinned runner's
`totals:` precedence, with `summary:` only as its documented fallback. Inputs
must be confined, non-aliased and hash-bound. Historical unmarked or scalar
verdicts cannot satisfy the required method.

Every comparison retains exact median and lower/upper residual fractions.
All upper bounds must be nonpositive to pass. A positive lower bound establishes
a regression; bounds spanning the boundary are `inconclusive`. If any comparison
is inconclusive, the whole measurement remains inconclusive even when another
comparison confirms a regression. Uncertainty and invalid evidence block before
owner-exception routing; neither becomes a pass or an accepted regression.
The summary and dashboard preserve these distinctions, sample counts and bounds
without inventing scalar baseline/candidate pairs. Raw evidence and failed
reports remain downloadable.

A valid `passed` result needs no exception. For `regression`, measurement first
reads the public GitHub REST definition of the `performance-exception`
environment without a token and validates it with `bend2-perf admission
environment`: its name must match, required reviewers must be exactly one
`User` whose numeric ID is the repository owner's, and administrator bypass
must be disabled. A missing environment, rejected/timed-out REST request,
unprotected environment, extra/wrong reviewer, or invalid definition fails
measurement; it is not a numerical-regression classification eligible for review.
Owner self-review is allowed because the owner is often the workflow actor,
but a separate explicit environment approval is still required.

Before scheduling review, measurement also reads this workflow run's public
approval history and records the count of `approved` entries matching both the
owner's numeric user ID and the exact validated environment ID. Malformed
history or a failed API request fails measurement. This pre-review count and
environment ID are published only with the fully validated classification.

Only then is the protected `review-exception` job scheduled. GitHub binds its
manual approval to the workflow run and commit. The job has no token
permissions, secrets, checkout, artifact download, or PR-code execution.
It reads the same run's public approval history and requires the matching
approved count to have increased before logging accepted source/run metadata.
Approval history has no attempt/timestamp binding, so counting fresh entries
rather than trusting any existing entry rejects stale approvals on reruns.
Both review and final admission require the saved measurement attempt to match
the current workflow attempt. Partial reruns reusing earlier measurement or
review results fail closed; rerun all jobs to collect fresh admission evidence.
Entry order is irrelevant. A removed reviewer rule, auto-created unprotected
environment, bypassed review, different environment/reviewer, missing fresh
approval, or malformed/API-failed history cannot be mistaken for acceptance:
protected-job success alone is not proof of owner approval.
The owner reviews the bounded acceptance report required by the
[ADR](adr-tiny-performance-gates.md) before approving. Until that approval,
regression admission remains pending, not green.

The token-free `performance admission` job always evaluates the dependencies:
it passes only for successful validated measurement plus `passed` with no
review, or `regression` with successful protected review. Rejection,
cancellation, missing results, and all other combinations fail admission.
Native latency and quality checks remain independent requirements. Branch
protection must require `performance admission` rather than interpret a green
measurement check as policy permission; environment protections must stay
configured as above.

No approval is universal or inferred from an earlier acceptance. In particular,
the accepted ordinary cache exception for PR #42 does not authorize PR #43 or
any later candidate/run. Existing historical failed compares remain failed.

## Collection lifetime and artifact publication

The ordinary and qualification collectors run under `bend2-perf workflow
supervise`. Each invocation owns a new POSIX process group. Linux supervision
also adopts orphaned descendants as a child subreaper and reaps only that
owned group, never unrelated children. A leader exiting does not prove its
writers have stopped. Unexpected surviving descendants make collection fail,
even when their leader returned zero and subsequent cleanup succeeds.

The measurement deadlines remain 210 minutes for ordinary paired collection
and 330 minutes for qualification. Their outer steps reserve two additional
minutes for termination, reaping and report handling, not additional samples
or measurement time. Deadline expiry remains a nonzero collection result;
TERM is followed by bounded KILL escalation when needed. Unknown process
states count as present, not absent.

The lifecycle receipt is checkpointed outside the measurement output, because
collectors require fresh output directories. After collection, the always-run
publication step copies it to `collection-lifecycle.json`. A pending receipt,
incomplete cleanup, inconsistent lifecycle or never-spawned command cannot
authorize raw publication. Completed cleanup after a timeout or interruption
can permit evidence retention, never a successful measurement verdict.
Sealing independently checks that the recorded group is absent, including
zombies, before walking the raw tree and again before publication.

`bend2-perf workflow seal --source ROOT --output DESTINATION` creates an
independent, write-finished snapshot. Publication currently requires Linux GNU
and atomic `renameat2(RENAME_NOREPLACE)`; unsupported platforms fail closed.
Existing destinations and overlapping source/output paths are never replaced.
Links, special files, changed inputs and copy errors are rejected rather than
followed, skipped or retried into apparent success.

A committed `sample.json` identifies a completed measurement. Its files retain
their original `raw/...` paths. Uncommitted samples are retained only under
`incomplete/raw/...`, with source-to-snapshot paths and hashes in
`snapshot-publication.json`. Original dataset, command and provenance metadata
are copied verbatim. This diagnostic namespace cannot supply missing samples
or make partial coverage admissible.

Cleanup or copy failures publish only a bounded failure envelope, without
partially copied raw evidence, and return nonzero. Successful snapshot
publication says nothing about the measurement verdict. The original full
105-pair, workload, source, uncertainty and admission checks remain authoritative;
the final admission job also requires the measurement job itself to succeed.
No permissions, approval, benchmark input, metric or numeric allowance changes.

Local Linux correctness checks may exercise the supervisor with controlled
child processes and the sealer with fixture files. They are not hosted
Callgrind measurements, performance admission or a semantic compiler oracle.

## Historical single-pair calibration

The historical raw calibration protocol ran on host Ubuntu 24.04, like
`performance / compare`, with Rust 1.98.1, Iai-Callgrind runner 0.16.1 and pinned
Valgrind 3.27.1. Its observed false alarms motivated the separately qualified
fixed-series method below. The current
[`performance-calibration` workflow](../.github/workflows/performance-calibration.yml)
uses that qualified protocol, not the old five-pair schedule.

The old protocol used seven discovery jobs and three independently assigned
validation jobs, each with five balanced series. Each job built five isolated
Cargo targets: source-identical A/B, a layout control, duplicate inlay-query
work, and a 512 KiB initialized allocation. Every measurement used a fresh
benchmark process and output directory.
All inlay variants use the same non-inlined query helper with blackboxed inputs.
The allocation helper is retained in every variant and called only by its control,
after the query result is produced. These calibration-only fences isolate control
costs from caller-dependent query inlining; they are not production benchmark changes.
Temporary harness copies retain a named layout function through its address in
setup, without executing it. `nm` must find the symbol, the layout variant must
have at least 1,024 bytes of code, and emitted Callgrind profiles must not contain
execution of that probe. Checked-in Rust and benchmark fixtures remain unchanged.

The collector preserves source manifests, binary hashes, symbol identities,
execution order, environment/host metadata, stdout/stderr, and raw Callgrind
profiles. Counts come only from that process's JSON output, never stale summaries.
Artifacts retain successful and failed measurements for 14 days. Missing,
incomplete, escaping, or incomparable evidence fails report generation.
The reporter reparses retained stdout with the collector's fresh-summary validator:
baseline identity, executed binary, workload identities, and counts must match
`data.json`. Merely retaining a file with the right name is insufficient evidence.

The historical raw-report method learns a per-workload/event cache floor only from positive discovery
A/A deltas, combined with the unchanged active allowance for each comparison.
Holdout, layout, and positive controls never train the proposal. Instruction
limits remain 2%; both positive controls must exceed them on all three validation
inlay workloads. A/A exceedances remain visible rather than being retried away.
Proposals are never automatically installed.

The strengthened raw-control gate required `--require-control-validation`.
Its verdict required complete seven-job discovery / three-job validation /
five-pair coverage, no source-identical A/A exceedance of the active limits in
either role, and detection of every held-out work/allocation control. Reports
were written before a failed check exited; raw evidence and individual failures
remained available. The failed controls below were not accepted regressions or
permission to widen limits. The historical CLI reader can still report partial
retained datasets, with failed control validation displayed explicitly.

The version-only 0.6.2 candidate exposed why this check is necessary.
[Calibration run 38682489026](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38682489026)
used exact merge source `0993cea112e7ea2726d26ad23992fcca6b64259e`.
Its held-out unchanged-source comparisons exceeded the instruction limit
13 times and the cache limits twice in 2,295 event comparisons. Positive controls
were still detected. Five processes of one unchanged executable in validation-03
measured 590–620 instructions for the small identifier query and
5,459,801–5,673,275 for burst invalidation. Its SHA-256 was
`b8c22daf2fbea711353b2a76a6bbc02b7ca1f5e90cc4010890b4ebf6cd7eb531`.
These are calibration-harness observations, not permission to reclassify the
ordinary candidate comparison or claim a production slowdown.

A report-only fixed-five median prototype still exceeded active limits for
unchanged-source controls: two discovery incremental-invalidation cases and
one held-out small identifier query. It was not adopted. Pooled samples remove
those particular exceedances, but that finite, dependent sample is not evidence
for changing the admission estimator. The production benchmark, limits,
baseline collection, and no-retry policy remain unchanged.

Reported p50/p95 are empirical nearest-rank values, not tail-confidence claims.
Comparisons share an A baseline within each series; series share builds within
each job. The calibration-only harness and diagnostic layout changes can affect
optimization, so neither layout differences nor control costs are assumed to be
pure measurement noise.

The [initial CI run](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37013864883)
collected 250 full-suite invocations (8,250 workload profiles). Its held-out A/A set had six instruction-limit
exceedances and no cache-limit exceedances across 495 comparisons per event.
The initial 32 KiB allocation control did not exceed the instruction limit on
medium/large workloads; the sensitivity check rejected the run. The control was
increased to 512 KiB, without changing gates or discarding the initial artifacts.
These initial observations do not justify increasing cache allowances.
The [512 KiB pilot](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37016883461)
also failed sensitivity: large inlay changed from 587,613 to 524,465 instructions
despite the allocation. Its held-out A/A set had five instruction exceedances
and no cache exceedances. This rejected pilot motivated the shared non-inlined
measurement helpers; it was not retried away or used to train allowances.

The [isolated-control CI run](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37024700423)
passed with all 250 full-suite invocations and 8,250 workload profiles. Both
positive controls exceeded the unchanged 2% instruction gate in all 45 validation
workload/series combinations each. Minimum increases were 83.19% for duplicate
query work and 8.42% for allocation. Held-out A/A had two instruction-limit
exceedances out of 495 event comparisons, and zero of 495 for each cache event.
The discovery-only proposal did not remove those instruction exceedances; it
never changes the instruction gate. This finite sample supplies no evidence that
cache allowances need increasing and does not establish tail reliability.

The historical protocol used the Rust `bend2-perf calibration collect` and
`calibration report` commands from [`tools/perf`](../tools/perf), with independent
work/output directories. Its 7/3/5 data remains historical evidence, not
qualification of the active estimator. Local fixture-based contract tests do
not collect measurements.

## Fixed-series controls and qualification

The [`performance-calibration` workflow](../.github/workflows/performance-calibration.yml)
enforces the prospectively qualified `paired-median-105-v1` protocol. It runs
for changes to the measurement infrastructure or benchmark scope and can be
manually dispatched. Passing controls qualify the measurement method; they do
not automatically admit a candidate, change a baseline or rewrite historical
raw verdicts. Ordinary admission separately measures the original sources.

The fixed experiment has three discovery jobs (`discovery-01` through
`discovery-03`) and three independently assigned validation jobs
(`validation-01` through `validation-03`), each collecting exactly 105 pairs.
Every pair measures the existing five variants (`a`, `b`, `layout`, `extra_work`,
`extra_alloc`) in the collector's balanced five-position order, with a fresh
process and isolated output for every measurement. The odd sample count is
divisible by five. There is no adaptive stopping, retry-for-luck, best-sample
selection, outlier deletion, or pooling of jobs, hosts or builds. Discovery
and validation both use the predeclared method; neither trains an allowance.

Every independently built variant now contains both benchmark executables:
`analysis` (96 workloads) and `reference_locations` (three workloads). Each
logical sample runs them in that order, in two distinct fresh processes with
separate output directories. The fixed experiment therefore contains 1,050
benchmark-process executions per job, without changing the 105 paired samples
used for each workload/event comparison.

Semantic extraction adds 45 workloads to the original 54-workload scope.
Qualification now requires all 99 workloads; the earlier 54-workload controls
do not qualify this expanded scope. Ordinary source admission still compares
every existing baseline workload and reports candidate-only workloads as
unpaired until a later baseline contains them. No existing workload, metric,
numeric allowance, or benchmark guard boundary changes.

Evidence format v2 binds each benchmark's actual `--list` identities, executable
hash and raw stdout/stderr to its frozen variant source. Both original harness
hashes are retained; the reference-location harness is not modified. Workload
sets must be complete, disjoint and consistent across variants. Reports expose
the benchmark scope and reparse both raw outputs before accepting merged counts.
Historical v1 evidence remains explicitly analysis-only and cannot be mixed
with v2 evidence or used to qualify the expanded all-benchmark scope.

For each job, variant, workload and event separately, the reporter sorts the
105 exact signed paired residuals:

```text
(candidate - baseline - active_allowance(event, baseline)) / max(baseline, 1)
```

The active allowance is unchanged: `floor(2 * baseline / 100)` for `Ir`,
and `max(ceil(3 * baseline / 100), 3)` for cache events. This changes the
estimator, not the numeric limits, benchmark workload, measured metrics,
guard boundaries or cache geometry. Exact integer/rational arithmetic retains
valid zero counters and full-width counts; floating-point display values never
decide the result.

Exact distribution-free binomial order-statistic bounds provide separate
one-sided 95% lower and upper confidence bounds for the population median.
Reports retain the sample count, bound ranks and confidence, exact median and
bound fractions, and every raw pair comparison. Ties remain in the ordered
sample: an upper bound equal to zero passes; a lower bound strictly greater
than zero is a regression; every other outcome is inconclusive. An inconclusive
result is not a pass.

These are conditional, same-job process-randomization claims, assuming
independent fresh-process observations for each comparison with fixed builds.
Variants share the same A baseline within a pair, and pairs share builds;
comparisons across variants, workloads or events are not independent. Bounds
are not simultaneous across metrics. Per-event lower-bound regressions are
not familywise significance claims. Requiring every upper bound to pass has
an intersection-union rationale only under the declared sampling assumptions.
The method estimates typical (median) per-process change, not p95, tail
reliability, whole-editor latency, or the probability of a future raw failure.
There are no tail-confidence or zero-tail-failure claims, even when every
observed A/A comparison passes.

Control validation requires complete 3/3/105 coverage of all 99 workloads across
both executables, enforced with `--expected-workloads 99 --require-all-benchmarks`,
and all of the following:

- Every source-identical `b` versus `a` workload/event comparison passes in
  every discovery and validation job independently.
- Both `extra_work` and `extra_alloc` are classified as regressions for `Ir`
  on all three inlay workloads in every held-out validation job. Merely
  inconclusive positive controls do not demonstrate sensitivity.
- Existing raw profile, stdout, source-manifest and binary-identity validation
  succeeds. Missing, duplicate, stale, malformed or unverified evidence cannot
  establish control acceptance.

The layout variant remains diagnostic, not an assumed cost-equivalent A/A
control or training data. No learned cache allowance applies to this method.
Raw one-pair failures remain visible even when a repeated comparison passes.
Insufficient coverage, inconclusive A/A, a missed positive control or invalid
evidence fails closed; JSON and Markdown diagnostics are retained before the
control-validation command exits unsuccessfully.

The workflow uses native Ubuntu 24.04, Rust 1.98.1, Iai-Callgrind runner 0.16.1,
and checksum/provenance-bound Valgrind 3.27.1, matching the raw collector.
Each collection job has a fixed 360-minute timeout; measurement itself remains
limited to 330 minutes. The 332-minute collection step includes a separate
cleanup reserve before sealed evidence and uploads. Missing pairs still fail
coverage; the reserve neither extends measurement time nor qualifies a scope.

The expanded-scope [run 39103430549](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/39103430549)
hit that 330-minute measurement limit on all six jobs, and all six raw uploads
failed. Its official report rejects `no data.json datasets found` as
`insufficient_evidence`. In-progress directory names are not completed-pair
counts. This is not qualification of the 99-workload scope or a reason to
reduce coverage, pool hosts or widen limits.
Actions are SHA-pinned; checkouts do not persist credentials; permissions are
read-only; only trusted manual main runs save tool/download caches, never
benchmark object caches.
There is no privileged PR-code execution, environment approval, release or
nightly schedule. The always-run report job downloads only this run and
attempt's six collection artifacts and checks full coverage even after
collection or download failure. Raw results, failure diagnostics, profiler
provenance and reports are retained for 14 days. Existing Rust control contracts
remain mandatory. The obsolete prospective-only workflow has been replaced by
this single qualified control workflow.

A maintainer can dispatch the controls on a reviewed source revision:

```bash
gh workflow run performance-calibration.yml --ref <reviewed-ref>
```

For retained artifacts, the equivalent report command is:

```bash
bend2-perf calibration report INPUT \
  --method paired-median-105-v1 --require-control-validation \
  --expected-discovery-jobs 3 --expected-validation-jobs 3 --expected-pairs 105 \
  --expected-workloads 99 --require-all-benchmarks \
  --json-output report.json --markdown-output report.md
```

Collection remains hosted-only. The default raw report method is retained for
historical datasets; the active workflow explicitly selects the qualified
method. A calibration report's `active_gates_changed: false` means the report
itself does not install policy or admit a candidate.

The fresh [full prospective qualification](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38720430197)
at `af128eb5ca571348b9b09d476368da4032e924ec` completed all six jobs, all 105
pairs and all 54 workloads. Source-identical A/A repeatability and held-out
positive-control sensitivity both passed, with no control failures. This
qualified the fixed method before the admission cutover; it is not performance
evidence for release 0.6.2 or permission to retry a rejected candidate unchanged.

The earlier [analysis-only experiment](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38692087771)
is insufficient evidence: only two of six fixed 105-pair jobs completed; four
hit their 180-minute job deadline, and their raw-artifact uploads also failed.
Its retained report rejects the missing coverage. Completed jobs are not reused
as part of a new experiment or promoted to complete-scope validation.

A [non-admitting infrastructure smoke](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38716125552)
exercised both genuine executables, all five variants and all 54 workloads on
two independent jobs with one pair each. Collection, raw-evidence validation
and scope enforcement passed. Its control verdict remains failed: coverage is
insufficient and raw single-pair A/A exceedances remain visible. A green smoke
workflow is not repeatability validation or candidate admission.


## Functional-fix performance acceptance (PR #4)

The maintainer authorized adequate, expected regressions for the correctness
release in [PR #4](https://github.com/IlyaGulya/bend2-lsp-rs/pull/4).
This is a bounded acceptance of the observations below, not a threshold increase,
performance-improvement claim, or new automatic waiver mechanism. The normal
compare remains failed; its result is preserved rather than retried away.

The [final functional-source comparison](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37032755314)
at branch commit `290a09b` passed 23 of 33 existing workloads. Workspace
references changed from 1,885,916 to 2,083,170 instructions (+10.4593%).
Nine other workloads exceeded cache allowances by 4–8 events. The three new
parameter-annotation cases have no old baseline. Cold snapshot construction
and all other existing instruction gates passed.

The [raw-profile preservation run](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37034499320)
kept the same measured source and commands at branch commit `26d78dd`.
Workspace references measured 1,886,186 → 2,083,147 instructions.
Callgrind attributes 920,933 → 1,115,786 instructions to destruction of
`WorkspaceDb`: 194,853 of the total 196,961-instruction increase (98.93%).
The increase is principally allocator consolidation during fixture teardown,
not additional semantic scanning. The remaining measured work changed
965,253 → 967,361 instructions (+0.218%). This subtraction is diagnostic only;
the original total remains the authoritative benchmark metric. The precise
source of the changed heap topology is not established.

The source-identical calibration's maximum held-out workspace-reference
instruction increase was 0.0444%, with no exceedances. Thus the total +10.46%
is not accepted as ordinary A/A noise. The source retains existing snapshot
and binding layouts; the new annotation query inspects indexed local tokens.
No full-source feature scan or optional incoming-call optimization is retained.

The same CI comparison preserved seven paired real-LSP rounds, based on
`654ac603d8c411832b855429bfe3f977bfc8956b` and candidate merge SHA
`c5c96f7da21d4bf43bf829e51ab72f56447ed9c9`. Warm p50 remained about
0.089 ms; large open/edit p50 remained about 21 ms. During a large edit,
unrelated-hover p50 changed 0.975 → 1.179 ms (+0.204 ms), and p95 changed
1.102 → 1.236 ms. These finite samples do not certify tail reliability or
editor latency, but bound the observed process-level cost of the revision and
child-lifecycle correctness changes.

The accepted tradeoff is the measured fixture-teardown cost, small absolute
cache-event increases, and sub-millisecond busy-query overhead in return for
correct lexical navigation, revision readiness, watched import edges, and
owned-child cleanup. Global instruction/cache gates, baseline selection,
existing benchmark rows, fail-if-flaky policy, and native-release gates are
unchanged. This acceptance does not authorize later unrelated regressions.


## End-to-end LSP latency reports

The `performance / latency` matrix builds the pull request's base and candidate
native release executables on Linux, macOS, and Windows, on x86_64 and ARM64, in
**separate Cargo target directories**. Each platform runs the same candidate-side
[`bend2-perf native latency`](../tools/perf/src/latency.rs) harness and fixture against
both binaries on one runner. Results are compared only within that platform.
Seven fresh-process rounds alternate
baseline/candidate order; each workload has eight warm-up requests and 32 measured
requests per round. Busy workloads use one warm-up burst and one measured burst.
Initialization and readiness waits are outside the measured windows.

The seven workloads cover:

- warm hover, cross-file function definition, and completion;
- opening a roughly 1 MiB document through its first correct hover;
- replacing that document through hover of the latest revision;
- unrelated hover bursts during a large edit and a large open.

Generated documents include real ADT declarations so snapshot measurements cover
constructor-index construction as well as function analysis. The existing large
fixture is repeated four times without changing the Callgrind fixtures or gates.
Large transient documents are closed between samples rather than accumulated.
Each response is checked for the expected semantic result; stale revisions,
protocol errors, timeouts, and unsuccessful shutdown fail measurement.

Latency starts before the request frame is written and ends when the full response
body has been read, before client JSON parsing. Open/edit-to-hover windows also
include the preceding notification frame transfer. Client frame serialization,
process initialization, editor rendering, and real Bend compiler execution are
excluded. The harness selects an unavailable compiler and isolates `PATH`, home,
library, metrics, and tracing settings; revision-specific diagnostics establish
readiness outside timing. These are controlled analysis/protocol workloads, not
claims about every editor or compiler workload.

[`bend2-perf reports latency`](../tools/perf/src/reports.rs) computes nearest-rank
p50/p95 for each round, then the median of the **round percentiles**, not pooled
requests. JSON includes every round's percentiles and paired deltas. Measurements
must have identical workload digests, environment, round counts, and sample counts.
Raw files record nanosecond samples and binary/harness/fixture SHA-256 identities.
Completed latency rounds are checkpointed to both raw files. Collection failures
retain those samples with `metadata.collection_status = "failed"` and an error;
interrupted collections remain `"running"`. The report command rejects both
states, so partial evidence cannot become a successful comparison.

The job publishes a Markdown GitHub job summary and an `lsp-latency-*` artifact
containing baseline/candidate samples, JSON/Markdown comparisons, and source
revisions. **Numeric latency changes are report-only** while runner noise is
being characterized; even a large slowdown does not fail by a latency threshold.
Malformed, incomplete, or incomparable measurements do fail. The existing
blocking Callgrind limits remain unchanged.

### Hosted-only measurements

All new performance measurements run on hosted CI, not on a local workstation
or its Docker container. Local correctness tests for the transport, comparators,
and report validation remain allowed; they are not performance evidence.
Collectors reject non-CI measurement invocations. Defaults remain seven rounds,
32 samples, and eight warmups; do not run builds/tests concurrently with collection.

All performance tooling is Rust in the separate, unpublished `bend2-perf`
workspace crate: portable JSON-RPC transport, collectors, reports, active
comparator, and calibration. CI builds it in a separate target directory before
collecting anything; it is not linked into the production server or measured
analysis functions. Release/install scripting remains outside this perf tooling.

The same six-platform matrix runs
[`bend2-perf native discovery`](../tools/perf/src/native.rs) against the
already-built binary pair. Deterministic 10/1000/10000-file workspaces distinguish
protocol initialization from complete unopened-file discovery, warm feature
queries, and dependency revisions. A base revision without discovery is reported
as scope-incomplete; its partial references/symbols are not equivalent-work
latency comparisons. Candidate completeness and correct public LSP results are
required, independently of numeric report-only latency changes.

The collector requests local parameter completion immediately after opening an
independent untitled buffer, before waiting for discovery. The raw result and
`isIncomplete` flag are preserved: a true flag establishes server-reported partial
discovery, while false or a legacy array does not establish overlap. There is no
numeric completion threshold or deterministic scan barrier.
The probe and later warm buffers use diagnostics-clear acknowledgements for
`didClose` before disk-only symbols and root-removal observations; a notification
write alone does not establish that a buffer's ownership has been released.
JSON checkpoints use buffered writes to a temporary sibling followed by atomic
replacement. Interrupted serialization preserves the preceding valid checkpoint;
an incomplete collection still cannot become a successful comparison.

Process resident-memory observations retain timestamped samples every 20 ms,
cold sampled peaks, retained state, and observations immediately and 100 ms
after workspace root removal. Candidate workspace symbols must become empty
after removal. Sampled peaks are lower bounds; exposed kernel high-water marks
cover the process lifetime. Resident memory may remain high after snapshots are
freed because the allocator retains pages.

- Linux reads `VmRSS` and `VmHWM` from the target PID's `/proc/<pid>/status`,
  with separate anonymous/file/shared resident and swap observations.
- macOS uses safe `libproc` wrappers for `proc_pid_rusage`: resident size is
  separate from physical footprint and wired memory. Its resident high-water
  mark is explicitly unavailable, not replaced with a footprint peak.
- Windows uses safe `winsafe` wrappers: working set and peak working set are
  separate from private commit and paged/nonpaged pool usage. Private commit is
  not RSS or pagefile occupancy.

Native field definitions and byte units travel with the report metadata.
Missing required resident samples/checkpoints make the report incomplete;
optional unavailable metrics stay null. No values are inferred from DHAT totals,
and no numerical comparisons are made across native targets.

Allocation profiles use separately built optimized, symbolized `dhat-heap`
executables for the real LSP lifecycle and the existing line-index/folding
examples on all six native platforms. Raw allocation profiles are distinct from
RSS observations; unavailable RSS fields are null, not zero. Profiles are
candidate-only observations, not fabricated baseline comparisons. Artifacts bind
source revisions, binary/harness/dataset digests, commands, toolchain, and runner
identity. Missing/malformed evidence and semantic failures fail CI.

The quality workflow also runs a correctness-only repeated lifecycle stress:

```sh
cargo run --locked --package bend2-perf -- lifecycle-stress \
  --binary target/debug/bend2-lsp --output target/workspace-stress/report.json
```

One real LSP process runs three cycles over the same deterministic 10,000-file
workspace: open, unsaved dependency edit, watched deletion/recreation, close,
root removal, rapid superseding root changes, and readdition. Exact symbol and
reference sets, current hover signatures, and definition locations are checked
at each applicable transition. All requests keep the existing transport timeout.
Atomic JSON checkpoints retain completed observations and the last response on
failure; CI uploads this evidence for 14 days. RSS checkpoints are report-only,
with no memory or latency admission threshold. This correctness command may run
locally; hosted performance-measurement restrictions remain unchanged.

The separate `workspace_lifecycle` integration test repeats three 10,000-file
import/open-root cycles in one `WorkspaceDb`. Weak handles verify retirement of
observed `DocumentSnapshot` payloads, preservation of independently open/shared
documents, fresh disk reloads, and stable file identities. This is snapshot
liveness proof, not proof that every allocation is freed or allocator pages are
returned. The protocol stress covers discovered-folder removal/readdition;
the direct DB test uses public import/open-root APIs instead of exposing private
discovery state. Runner checkout metadata and the supplied binary digest are
recorded separately, without claiming a digest alone attests build provenance.

macOS symbol copies materialize Cargo's dSYM aliases. A bundle retains its
single original DWARF member, including Cargo's crate-name/hash form; component
hashes and link-free structure remain required
([Cargo output naming](https://github.com/rust-lang/cargo/blob/f96969bb236ab59543a5fdf5f131c874cece23aa/src/compiler/build_runner/compilation_files.rs)).
DHAT block counts at a byte peak are not maximum block counts: `mbk` need not
bound `gbk` or `ebk`. All three remain bounded by total allocated blocks, while
the existing byte-size invariants remain enforced
([DHAT 0.3.3 source](https://docs.rs/crate/dhat/0.3.3/source/src/lib.rs)).

The profiled LSP retains the 30-second JSON-RPC deadlines, then allows up to
300 seconds for child finalization, matching the existing native DHAT example
bound. Ordinary latency runs retain their original exit deadline. DHAT resolves
backtraces before printing its summary or creating the output file; shutdown
stage and exact child-exit evidence are retained separately. The first Windows
x86_64 failure's empty stderr and missing profile were consistent with slow
finalization, not proof of its cause; successful hosted profile generation is
required before treating this path as verified.

The canonical Linux/x86_64 Callgrind gate and the calibration workflow remain
unchanged. Callgrind is unavailable on the supported macOS/Windows runners;
native timing/allocation reports do not replace its instruction/cache metrics.
Workflow/enforcement changes require the `policy-approved` pull-request label.


### Unified hosted CLI and reports

The workspace Cargo alias runs the unpublished Rust tool:

```sh
cargo perf doctor
cargo perf compare --base main --candidate HEAD
cargo perf profile discovery-10000 --cpu
cargo perf profile discovery-10000 --heap
cargo perf profile discovery-10000 --native --native-kind cpu
cargo perf open <run-id-or-downloaded-directory>
```

`doctor` inspects prerequisites without elevation, installation, authentication
changes, or measurements. Its default checks local Rust/Cargo, `gh`, repository
identity and existing authentication. Optional `--backend cpu|heap|native`
inspects that local diagnostic tool; those tools are not required locally to
dispatch hosted collection.

`compare`/`profile` resolve exact pushed source SHAs. `HEAD` means the committed
head, not uncommitted edits. The workflow definition defaults to the repository's
default branch; `--workflow-ref` can select a published branch or tag after the
workflow is registered on the default branch. A unique request ID selects the
exact run; the tool never downloads an arbitrary latest run. `--target` defaults
to all six triples or selects one explicitly. Existing output directories are
never overwritten.

The diagnostic workflow supports `compare`, `cpu`, `heap`, `native`, and `full`.
Ordinary PRs retain clean native latency/discovery/process-memory collection and
the source-bound, fixed105 Linux x86-64 Callgrind admission. Infrastructure-source changes
add six-target backend verification and native tooling contract tests.
The weekly/manual full workflow additionally retains every original thirteen
DHAT workload and profiles all four shared scenarios: `discovery-10`,
`discovery-1000`, `discovery-10000`, and `latency`. CPU, allocation, and native
traces use separate processes, never the clean latency process.

CPU profiling pins samply 0.13.1 to source revision
`da75c28f367454c621e690eeb4e44ec2ebb29a78`. Windows ARM64 additionally applies
`scripts/patches/samply-windows-arm64.patch` (SHA256
`37bc36692372474829e099ce5faad0ca765184c76c01b31544fae5bd7365de51`) to join
matching SampleProf records with their kernel/user StackWalk halves without
discarding CPU deltas. Other platforms use the unmodified pinned source.
The installed ARM64 sampler sidecar binds the upstream revision, patch digest,
target, and executable SHA256; doctor and capture validate that identity and
retain the modified-source provenance.
Native diagnostics use Linux perf, macOS Xcode Instruments, or Windows WPR with the pinned Microsoft
Windows Performance Toolkit. Native heap tracing is supported on macOS/Windows;
Linux native heap requests reject explicitly and point to `--heap` DHAT.
Missing permissions, symbols, target samples/allocation records, or complete
trace data fail collection rather than falling back to metadata-only success.
WPR's successful save exit status is not sufficient when its stop output reports
dropped events: the trace is retained, but collection fails as incomplete.
The ETL decoder also rejects header loss counters and lost-event notifications.
Windows ETL decoding uses the repository's streaming `bend2-etl-reader`, built
with Microsoft TraceEvent 3.2.8, .NET SDK 10.0.401 and runtime 10.0.12.
The SDK archive is SHA512-pinned; NuGet dependencies use a checksum-bound lock.
Doctor validates the executable, target, SDK and all six build-input hashes.
The hosted bootstrap disables persistent build servers so the temporary SDK's
DLLs are released before cleanup; cleanup failures still fail the job.
The reader processes the entire ETL to EOF and emits only header/lifecycle,
CPU sample and target-PID heap allocation records for the existing validator.
It does not serialize the whole recording through `tracerpt`, stop after the
first target record, or invent zero loss counters. Raw ETL and symbols remain
in the artifact; the existing 300-second decoder deadline remains unchanged.
Windows sampled CPU attribution uses the kernel event-class GUID and the sampled
thread's lifecycle ownership, not the event reporter's PID. Raw lifecycle
payloads require the documented version, pointer width, and complete payload.
Installed native CPU/Heap profiles are exported before configuration; provider,
keyword, stack, and file-mode semantics are preserved. Collectors use 1 MiB
buffers: 256 system; CPU profiles use 64 ordinary-event buffers, while Heap
profiles use 256 ordinary-event and 1024 heap-event buffers. Lossless stack caches
remain 16 MiB per system/ordinary collector and 64 MiB for the heap collector.
These are explicit tuning choices, not Microsoft recommendations; hosted capture
must still demonstrate zero event loss and complete target-process evidence.
Source/configured profiles and collector logs
remain in the artifact ([WPR collector definitions](https://learn.microsoft.com/en-us/windows-hardware/test/wpt/collector-definitions),
[stack caching](https://learn.microsoft.com/en-us/windows-hardware/test/wpt/stackcaching)).

Before stopping WPR, capture its owned collector status and `xperf -loggers`
output. These runtime counters distinguish rejected events from lost log buffers
and show the actual pool sizes; configured counts alone cannot attribute loss.
The stop and decoder loss checks remain fatal. Counter-command failures are
reported only after the owned recorder has been stopped, so evidence collection
cannot strand an active WPR session.

The ARM64 WPR failure in
[run 38155518804](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38155518804)
reported 6,412,765 lost events and zero lost buffers; its startup counters do not
identify the rejecting logger. The separately sourced
[run 38204311856](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38204311856)
completed 36,676,901 allocation records with byte-identical WPR configuration,
the requested effective buffer pools, and zero pre-stop and final loss. This is
successful capture evidence, not a causal repair of the original loss. No pool,
provider, stack or deadline change is justified by those successful counters.

The next preserved ARM64 failure in
[run 38213507627](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/38213507627)
identifies both rejecting collectors before stop: 22,855,441 Event and 31,065,836
Heap events, totaling 53,921,277. System loss and every log-buffer loss counter
are zero; stop adds no lost events. All original fixed pools were honored.
Late free buffers cannot establish whether earlier event-rate peaks exhausted
the pools. Only the Heap profile's two rejecting pools are increased above;
system and CPU pools, providers, stacks, caches, workload and deadlines stay
unchanged. Preallocated buffers rise from 832 to 1536 MiB, with the same 96 MiB
of stack caches, on the standard 16 GB Windows runner. This is a bounded repair
candidate, following
[ETW buffer-tuning guidance](https://learn.microsoft.com/en-us/windows/win32/api/evntrace/ns-evntrace-event_trace_properties),
not proof of the exact transient cause or of sufficient peak capacity. Acceptance
requires a new exact-source capture with honored pools, zero owned and final
loss, complete payload, and clean target shutdown. A failed capture does not
authorize an unchanged rerun or an automatic second size increase.

Decoder command arguments use ordinary Win32 drive/UNC paths; canonical paths
remain the artifact identity.
Windows heap tracing is prepared before process creation and restores the prior
IFEO state on normal failure or cooperative cancellation. Hard process kills
cannot promise in-process cleanup.

The macOS diagnostic workflow explicitly authorizes headless `samply` capture
through `sudo -n`; only the sampler and its owned cleanup commands are elevated.
`BEND_PERF_MACOS_SAMPLY_ELEVATED=true` is honored only in hosted CI without a
controlling terminal. Missing authorization fails rather than opening a dialog
or silently retrying with privileges. `doctor` and local trace viewing do not
elevate. Locally signed debugger-entitled tools otherwise require an administrator
authorization dialog ([Apple debugger entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.debugger)).
Sampler finalization targets the uniquely identified executable inside the
owned macOS process group, not a possible sudo monitor. Windows uses a
collector-console CTRL_BREAK event so inherited CTRL_C-ignore does not lose
the stop request ([console control events](https://learn.microsoft.com/en-us/windows/console/generateconsolectrlevent)).
Instruments waits for a notification-registration barrier before starting its
recorder, then requires the actual tracing-started notification and a live
recorder. Linux ARM64 software-clock DWARF capture requests the complete
baseline register set, excluding hardware-only SVE VG.

Before creating the cold LSP, native Instruments preparation starts a one-second
Time Profiler run on `/usr/bin/true` to initialize Apple's tracing and
authorization services, under the existing 300-second preparation-command bound.
Its trace and command logs are preparation evidence, not scenario samples.
The actual LSP capture still requires its own tracing-started notification
within 60 seconds.

If an owned Instruments recorder is still finalizing 280 seconds after its stop
request, collect one one-second `/usr/bin/sample` stack snapshot with the existing
10-second sampling-command bound. Keep the original 300-second finalization
deadline; sampler failure does not reset it or admit an incomplete trace.
The stack and command outcomes are retained in the profile artifacts before
owned cleanup. This is failure diagnosis, not clean latency evidence or a fix
for the recorder's internal stall.

Native Allocations captures set `MallocNanoZone=0` only in their LSP child and
retain that override in `profiler.target_environment`. Apple's
[libmalloc source](https://github.com/apple-oss-distributions/libmalloc/blob/libmalloc-715.140.5/src/nanov2_malloc.c)
shows that its in-process nano-zone enumerator allocates from the helper zone;
the retained Intel attach stall contains this path alongside the target's tiny
allocator. Disabling nano removes that allocating enumeration path without
changing production binaries or weakening allocation-record validation.
This is a source-supported tracing compatibility configuration, not an
Apple-documented public API. Small allocations use the scalable allocator
instead, so these native heap numbers do not describe the default allocator.
Clean process-memory/latency and DHAT runs do not receive this override.

macOS Allocations launches the retained, symbolized server directly under
Instruments rather than injecting into a server already discovering its workspace.
Only that separately built binary enables the default-off `native-trace-stdio`
feature. Its first startup action opens precreated FIFO endpoints in the owned
`BEND2_LSP_NATIVE_STDIO_DIR`; it does not spawn or exec a wrapper. Standard output
opens before standard input, and both remain blocked until the owner observes the
registered tracing-started notification. Protocol traffic uses those FIFOs;
diagnostics use a separate regular file. The directory and endpoint checks reject
symlinks, wrong kinds and replaced inodes. Ordinary server, timing, CPU and DHAT
builds do not enable this feature.

The owner registers a kernel process-exit observer before pairing the pipes.
Clean shutdown, nonzero exit and signal termination are attributed to the actual
server PID independently of the recorder's exit status; the recorder owns target
reaping. No held writer fabricates stdout EOF. Successful launched heap captures
finalize the recorder after protocol shutdown and require an exact launched target
and real Allocations payload. Raw traces, full recorded stacks, retained symbols
and the existing request, readiness and finalization deadlines remain unchanged.

An attached scenario owner finalizes its collector before terminating a failed
LSP target; ordinary transport clients retain immediate failure cleanup.
Responses received after their request deadline remain timeout failures even
when already buffered, so they follow the same failure-diagnostic path.

For timed-out macOS DHAT and native heap requests, preserve a one-second
`sample` capture of the still-live target before cleanup, with a ten-second
command bound. Sampling does not retry the request, increase its thirty-second
deadline, or turn a failed profile into a successful measurement. A missing
historical stack is not evidence of either profiler overhead or a server defect.

Resolve the actual profile kind from the selected backend: `--heap` means heap,
`--cpu` means CPU, and `--native-kind` applies only to native tracing. The session,
manifest and backend preflight use the same kind. The preserved Intel DHAT
timeout in run 38225001754 had no live sample because the session incorrectly
used the native-kind default `cpu` despite its heap manifest. Correcting dispatch
restores the timeout diagnostic; it does not explain the thirty-second cold
workspace-symbol timeout or authorize a retry of the failed source.

Each target's `artifact-manifest.json` records expected collectors, source and
tooling SHAs, target/request identity, statuses, file sizes and SHA256 hashes.
Downloads are validated with confined relative paths. Interrupted/failed runs
keep their raw data and explicit failure state. The report writes `index.html`,
`unified-report.json`, and `summary.md`; an incomplete/failed/regressed report
still produces diagnostic files before returning nonzero. CI uses
`dashboard <root> --target-only` for a single uploaded bundle; that mode explicitly
does not claim complete aggregate matrix coverage.
The unified JSON retains summary measurements and links to validated raw
discovery evidence rather than embedding a second copy of its full payload.
Raw files, checksums, pairing checks, and completeness validation are unchanged.
CI records the report command before inventory checksums are finalized, so
rendering does not invalidate the command log. Job summaries point to the
uploaded full report instead of embedding it beyond GitHub's 1 MiB limit.
HTML/Markdown show generated discovery inputs through the full verified JSON
artifact catalog rather than creating thousands of source-file links in the
page. Every source path/hash and original file remains retained. Repeated
identical scope warnings appear once visually; per-round evidence is unchanged.
HTML renders each round's resident samples as its timeline instead of embedding
the same arrays again in JSON details. Warm detail panels contain request
distributions and initial completion, not another copy of memory observations.
Full samples, manifest inventories and expected paths remain in the verified
JSON catalog and original reports; no collection or validation input is removed.

The canonical Callgrind evaluator retains `callgrind-policy.json` from the same
authoritative comparison, including workload metrics, failed gates, source/run
provenance, and input identities. Reports require this verified verdict and
actual command exit codes to distinguish numerical regression from collection
failure. A failed GitHub workflow is attributed to regression only when retained
job/step conclusions establish that its only failed step is that source-bound
comparison. In the split admission workflow, a valid measurement can succeed
while that producer step retains its failed outcome and regression verdict;
admission is a distinct decision, not evidence that the metric passed.
Historical bundles without authoritative verdicts or attribution evidence
remain conservatively incomplete/failed; no pass is inferred.
Retained per-metric outcomes must also agree with the same authoritative
`within_limit` function used by the producer. Hash-bound numeric inputs and
internally consistent flags alone cannot establish a passing or regressed gate;
an outcome contradicting the unchanged limits is rejected as invalid evidence.

`cargo perf open <downloaded-directory> --cpu --scenario <scenario> --target <triple>`
opens a validated CPU trace through `samply load`, with its packaged binary/debug
symbol directories. This local command is viewing, not measurement; it requires
the pinned viewer installed and does not install it automatically. Native
`.trace`/`.etl`/`perf.data` artifacts keep their original viewers and formats.
Canonical Callgrind verdicts remain separate from collection success; a
diagnostic comparison without Callgrind evidence says so, not that the gate passed.

## Crate, runner, and build setup

Iai-Callgrind's published crate is currently **0.16.1**; upstream renamed the project and packages to **Gungraun** starting at 0.17.0 ([crate release page](https://docs.rs/crate/iai-callgrind/latest), [upstream changelog](https://github.com/gungraun/gungraun/blob/main/CHANGELOG.md)). If retaining the `iai-callgrind` package/API, pin `iai-callgrind` and `iai-callgrind-runner` to the same version. The official install guide uses a dev dependency and requires `harness = false`; `main!` replaces the default harness, and the matching runner executable must be available on `PATH` (or `IAI_CALLGRIND_RUNNER`): [0.16.1 installation guide](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/installation/iai_callgrind.md) · [`main!` API](https://docs.rs/iai-callgrind/0.16.1/iai_callgrind/macro.main.html).

Iai-Callgrind requires debug symbols. The release profile strips symbols, so `Cargo.toml` sets `[profile.bench] debug = true` and `strip = false` to retain them ([manifest](../Cargo.toml#L63-L71), [prerequisites](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/installation/prerequisites.md)).

## Regression limits and baselines

Callgrind records `Ir` (instructions executed). Available cache events include `I1mr` (L1 instruction-cache read misses) and `ILmr` (last-level instruction-cache instruction misses); the `EventKind` reference defines cache events as requiring cache simulation (`--cache-sim=yes`) ([EventKind 0.16.1](https://docs.rs/iai-callgrind/0.16.1/iai_callgrind/enum.EventKind.html)). The documented default Callgrind metrics include cache-hit metrics, but for explicit instruction-cache miss limits configure cache simulation explicitly.

The active `performance` workflow retains these per-pair allowances:

- `Ir`: `floor(2 × baseline / 100)` additional instructions, equivalent to
  `candidate × 100 ≤ baseline × 102`.
- `I1mr` and `ILmr`: `max(ceil(3 × baseline / 100), 3)` additional misses.

The shared [`policy.rs`](../tools/perf/src/policy.rs) arithmetic remains
authoritative. The fixed105 estimator applies it to every source pair, then
uses exact signed normalized residual bounds as specified above. It measures
instructions and instruction-cache misses, not cycles, allocation counts or
editor latency.

Both source revisions must be clean, exact Git checkouts. The collector verifies
tracked source inputs, freezes independent copies, builds both benchmark
executables, records their actual `--list` inventories and rejects source or
binary mutation. Every baseline `(function_name, id)` is required exactly once
in each sample. Missing, duplicate or reassigned workloads and malformed counters
fail closed. Candidate-only workloads are listed without a fabricated zero
baseline. Ordinary admission follows the actual baseline inventory rather than
a hard-coded workload count; the control experiment separately requires its
predeclared complete 54-workload scope.

Inside the source-bound hosted job, after `workflow prepare` creates the
`paired-median-105-v1` envelope, the collector and reporter are invoked as:

```sh
"$RUNNER_TEMP/bend2-perf" paired collect \
  --baseline-source "$RUNNER_TEMP/paired-main" \
  --candidate-source "$GITHUB_WORKSPACE" \
  --work-dir "$RUNNER_TEMP/paired-work" \
  --output-dir "$PERF_ROOT"
"$RUNNER_TEMP/bend2-perf" paired report "$PERF_ROOT" \
  --json-output "$PERF_ROOT/callgrind-policy.json" \
  --markdown-output "$PERF_ROOT/callgrind-policy.md"
```

The workflow records both exits and always finalizes the input inventory before
admission validation. It explicitly supplies the required method, expected run
attempt and workflow SHA; the attempt published to downstream admission comes
from the validated result, not an independently relabeled environment value.
Each fresh Iai invocation saves to a unique isolated baseline and must emit
current-only `Left` counts. Existing saved summaries never supply a source pair.

The legacy `bend2-perf policy` reader remains available for historical scalar
artifacts and boundary contracts. It is not the active PR admission path.

### Active cache-event floor

Cache misses are discrete counts. A percentage-only gate is misleading near
zero: an increase from 2 to 3 is one event but appears as +50%. The absolute
floor applies only to `I1mr` and `ILmr`; `Ir` remains a relative +2% limit with
no absolute allowance.

The comparator implementation and tests are
[`tools/perf/src/policy.rs`](../tools/perf/src/policy.rs) and
[`tools/perf/tests/policy_contract.rs`](../tools/perf/tests/policy_contract.rs).
The tests run from `scripts/quality` and use temporary synthetic
`summary.json` files in Iai 0.16.1 shape. They cover `[candidate, baseline]`
ordering; selected-baseline filtering; 25→25, 25→36 with 11 new, 36→36,
36→35 missing, and 36→37 with one new; duplicate baseline and candidate IDs;
candidate-only `Left` summaries; hard errors for missing `Ir`/`I1mr`/`ILmr`,
absent pairs in paired summaries, and malformed paired metrics; cache `0→3`
pass and `0→4` fail; the `2→3`, `2→6`, `62→65`, `62→66`, `1000→1029`, and
`1000→1031` cache boundaries; and the unchanged `Ir` +2% boundary.

The active workflow is
[`performance.yml`](../.github/workflows/performance.yml). Changes to the
comparator and its tests require ordinary maintainer PR review; changes to
metrics, thresholds, or the baseline workflow still require maintainer approval.

The earlier 25-row saved-profile dry-run is not acceptance evidence for `.21`.
The fresh exact legacy comparison with baseline
`rbt21_legacy_4669c65_policy_fresh2` passed **25/25** under this policy on
2026-09-29. Full `./scripts/quality` passed, and local issue `.21` is closed.

## CI platform constraints

This is not intrinsically Linux-only: Iai-Callgrind requires Valgrind and therefore only runs on a platform Valgrind supports. Upstream's CI installation examples use the matching runner version, and its prerequisites list Linux distributions and FreeBSD for Valgrind installation ([prerequisites](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/installation/prerequisites.md), [runner install/CI guidance](https://raw.githubusercontent.com/gungraun/gungraun/v0.16.1/docs/src/installation/iai_callgrind.md)). A Linux CI job with Valgrind installed is the narrow, conventional gate; `apt-get install valgrind` is the upstream Debian/Ubuntu example.

The native macOS/arm64 host is not a supported Valgrind platform. The persistent
`rust` service in [`compose.yaml`](../compose.yaml) remains available for Linux
correctness checks, but local Docker runs are not current performance evidence.
New measurements use hosted CI; historical container measurements below are
retained solely as provenance ([Valgrind supported platforms](https://valgrind.org/info/platforms.html)).

The committed [`performance` workflow](../.github/workflows/performance.yml)
saves a `main` baseline and its benchmark-ID manifest from the pull request's
base SHA in ignored `target/iai`, then compares all manifest IDs and reports
candidate-only IDs. The workflow runs only for pull requests targeting `main`
and measures the selected benchmark group in `benches/analysis.rs`, not
end-to-end LSP latency.

## Legacy compatibility baseline (.21)

The 2026-09-28 comparison ran on Linux/ARM64 from legacy production SHA
`4669c65`; production sources in that worktree were unchanged. A temporary
benchmark-only adapter used the same current fixtures and benchmark IDs; its
source is not included in this six-path policy change. Fixture SHA-256:
small `d2f018dbc359ab0231fb19eb094c42a1298cd60a71416490fd05e3783dcab1f3`,
medium `d50c8644b6ad233da6606a77bdae8409326134f99838b726a50cbc042debf7e9`,
large `3a031f0c3ca50a4bf5e84955afb45edc2caf37b048af6f96fac07af8bdbb6472`.

The original legacy baseline is `rbt21_legacy_4669c65_fullcompat` (25
historically matching IDs). Its candidate counterpart,
`rbt21_candidate_4669c65_compat`, is preserved as historical data. The
2026-09-29 corrected comparison uses
`rbt21_legacy_4669c65_sparse_unicode`, with 25 IDs matching the current suite.
The Unicode position fixture is 40,623 bytes: the medium fixture plus a
4,096-`é` line. Earlier baselines remain in the persistent Docker `target/iai`
volume. The 22/25 run below is historical evidence under the former
percentage-only cache gate, not acceptance evidence for the active policy.

The adapter measures legacy `analysis` queries directly. It copies the old
server's private position helpers and single-document call-site scan from
`src/main.rs`; references use the old declaration-plus-identifier scan.
Per-case Iai setup runs each position conversion before Callgrind collection
for both implementations; the Unicode query ends at the 4,096-`é` line's EOF
and exercises sparse-checkpoint lookup. Snapshot construction remains a
separate cold workload. Twenty-five workloads match. Historical `N/A` applies
to the three cold snapshot-build rows and four workspace DB rows (initial build,
invalidation, cross-file references, burst revisions): the legacy server had
neither `DocumentSnapshot` construction nor the current indexed `WorkspaceDb`
API.

| Workload | Legacy Ir/I1mr/ILmr | Current Ir/I1mr/ILmr | Δ Ir/I1mr/ILmr | Historical gate |
|---|---:|---:|---:|---:|
| Semantic tokens — small | 150,442/98/95 | 20,471/8/5 | −86.39%/−91.84%/−94.74% | PASS |
| Semantic tokens — medium | 2,085,956/95/78 | 177,882/14/6 | −91.47%/−85.26%/−92.31% | PASS |
| Semantic tokens — large | 14,586,618/75/73 | 526,573/13/10 | −96.39%/−82.67%/−86.30% | PASS |
| Completion — small | 77,646/115/110 | 28,630/26/22 | −63.13%/−77.39%/−80.00% | PASS |
| Completion — medium | 674,328/135/116 | 182,461/28/22 | −72.94%/−79.26%/−81.03% | PASS |
| Completion — large | 4,874,244/150/131 | 621,154/37/30 | −87.26%/−75.33%/−77.10% | PASS |
| Identifier ranges — small | 70,891/40/40 | 786/15/15 | −98.89%/−62.50%/−62.50% | PASS |
| Identifier ranges — medium | 727,572/40/40 | 796/19/15 | −99.89%/−52.50%/−62.50% | PASS |
| Identifier ranges — large | 3,830,955/40/40 | 821/20/15 | −99.98%/−50.00%/−62.50% | PASS |
| References — small | 317,778/116/112 | 2,582/16/15 | −99.19%/−86.21%/−86.61% | PASS |
| References — medium | 110,222,535/116/112 | 57,540/24/19 | −99.95%/−79.31%/−83.04% | PASS |
| References — large | 2,586,256,972/116/112 | 169,400/23/19 | −99.99%/−80.17%/−83.04% | PASS |
| Call hierarchy — small | 692,457/117/113 | 1,856/17/16 | −99.73%/−85.47%/−85.84% | PASS |
| Call hierarchy — medium | 222,710,699/117/113 | 70,306/24/20 | −99.97%/−79.49%/−82.30% | PASS |
| Call hierarchy — large | 3,950,475,855/116/113 | 207,766/24/20 | −99.99%/−79.31%/−82.30% | PASS |
| Inlay hints — small | 48,032/62/59 | 9,441/65/61 | −80.34%/+4.84%/+3.39% | FAIL |
| Inlay hints — medium | 184,567/76/73 | 24,558/35/29 | −86.69%/−53.95%/−60.27% | PASS |
| Inlay hints — large | 693,650/74/71 | 71,611/36/33 | −89.68%/−51.35%/−53.52% | PASS |
| ASCII position conversion | 5,529/3/3 | 135/2/2 | −97.56%/−33.33%/−33.33% | PASS |
| ASCII offset conversion | 18,240/2/2 | 56/3/3 | −99.69%/+50.00%/+50.00% | FAIL |
| Unicode position conversion | 145,101/2/2 | 277/2/2 | −99.81%/0.00%/0.00% | PASS |
| Unicode offset conversion | 232,588/2/2 | 90,211/3/3 | −61.21%/+50.00%/+50.00% | FAIL |
| Folding — 100 lines | 43,205/69/68 | 13,194/21/19 | −69.46%/−69.57%/−72.06% | PASS |
| Folding — 1,000 lines | 409,314/69/68 | 121,694/25/21 | −70.27%/−63.77%/−69.12% | PASS |
| Folding — 10,000 lines | 4,095,720/108/92 | 1,206,359/26/23 | −70.55%/−75.93%/−75.00% | PASS |

The 2026-09-29 25-row comparison below was evaluated under the then-active
`Ir=2%`, `I1mr=3%`, and `ILmr=3%` percentage limits. Twenty-two rows passed;
three failed: small inlay hints missed the cache limits at 65/61 versus 62/59
events, while ASCII and Unicode offset conversion each rose from 2 to 3 cache
misses. The ASCII offset result is a strong instruction reduction (18,240 to
56 Ir, −99.69%); its failure is only one additional miss per cache event at a
2-event baseline. This historical 22/25 result motivated the active floor and
is not acceptance evidence for it. Production offset code was unchanged. The
fresh 2026-09-29 comparison used unmodified legacy production commit `4669c65`
with baseline `rbt21_legacy_4669c65_policy_fresh2`; the comparator reported
`25/25 matched workloads passed; 11 summaries did not match the selected
baseline.` Full `./scripts/quality` passed; `.21` is closed.

Exact ordered-output parity passed for all 1,600 medium and 4,800 large hints.
`analysis::inlay_hints` now returns each argument position and its parameter
name `TextRange`; `InlayHint::label` and the server adapter materialize the
same `"name: "` LSP label. The warm query benchmark measures the indexed
analysis query, not the adapter's per-response label `String` allocation.
Thus the query no longer allocates temporary parameter strings or output
labels, but label strings are still required when constructing the LSP reply.

Allocator-heavy costs moved out of the warm query. Before the change, the
medium and large profiles were dominated by deallocation and allocator
bookkeeping; after it, sorting hints is the largest measured cost center:

| Workload | Query Ir before → after | Dominant allocator costs before | Allocator costs after |
|---|---:|---|---|
| Medium | 343,400 → 24,558 | `_int_free` 159,750 (46.52%); `malloc_consolidate` 68,093 (19.83%); `free'2` 54,384 (15.84%); `unlink_chunk` 34,299 (9.99%) | `_int_free` 184 (0.75%); `_int_malloc'2` 175 (0.71%); `malloc` 92 (0.37%) |
| Large | 720,860 → 71,611 | `_int_free` 479,692 (66.54%); `free'2` 163,201 (22.64%) | `malloc` 92 (0.13%); `_int_free` 92 (0.13%) |

The after profiles put stable sorting at 11,200 Ir (45.61%) for medium and
33,600 Ir (46.92%) for large. Parameter names reuse the existing contiguous
binding storage; the cold `IndexedSymbol` metadata adds 16 bytes per symbol
without adding allocation blocks. The measured snapshot deltas are recorded
below. This warm query reduction is proportionate to the measured cold and
memory cost; the `.21` policy decision is closed.

The historical comparison used a temporary adapter and saved Iai/Callgrind
data from the Linux container. Neither artifact is included in this six-path
policy change, so these measurements are historical evidence rather than a
standalone reproduction recipe.

The candidate comparison stores per-benchmark JSON summaries and Callgrind
outputs under the shared `/workspace/target/iai/bend2-lsp/analysis/` volume.

## UTF-16 position mapping and snapshot memory

`LineIndex::position` binary-searches `line_starts` to locate a line. ASCII
lines then compute the character offset as `offset - line_start`; a packed
bitset uses one bit per line to select that path. Non-ASCII lines use sparse
checkpoints every 32 Unicode scalars: each stores an absolute byte offset and
line-relative UTF-16 units, then conversion encodes at most 31 scalars after
the preceding checkpoint. The checkpoint vector contains entries only for
non-ASCII lines. No unsafe code is used.

The repeatable profile harness performs 100,000 EOF position queries after
building a `LineIndex`; its Unicode input appends one 4,096-`é` comment line
to the medium fixture. DHAT 3.19.0 reports whole-process allocation totals,
including the input builder and runtime:

| Mode | Input bytes | Total allocated | Peak live heap |
|---|---:|---:|---:|
| ASCII position | 2,552 | 4,797 bytes / 21 blocks | 1,736 bytes / 3 blocks |
| Unicode position | 40,623 | 128,838 bytes / 35 blocks | 75,790 bytes / 7 blocks |

The Unicode comment contributes 128 retained checkpoints (2,048 bytes of
checkpoint payload at 16 bytes each); 100,000 repeated queries add no
allocations. The position-mode peak includes the process and input, not only
the line-index vectors.

The final matched Callgrind run uses prewarmed conversion inputs; the Unicode
case ends at the EOF of the 4,096-`é` line, exercising sparse checkpoint
lookup. Values are `Ir/I1mr/ILmr`; conversion rows compare the legacy adapter
to the current implementation:

| Query | Legacy | Current |
|---|---:|---:|
| ASCII position | 5,529 / 3 / 3 | 135 / 2 / 2 |
| ASCII offset | 18,240 / 2 / 2 | 56 / 3 / 3 |
| Unicode position | 145,101 / 2 / 2 | 277 / 2 / 2 |
| Unicode offset | 232,588 / 2 / 2 | 90,211 / 3 / 3 |

Cold `DocumentSnapshot::new` profiles use one snapshot per process; source
sizes are the same small/medium/large fixtures as the Callgrind suite. These
whole-process figures include parser/index storage, the owned source, and
runtime allocations, so they are comparative process measurements rather than
an isolated `DocumentSnapshot` retained-size measurement:

| Snapshot input | Total allocated before → after | Peak live heap before → after | Total/peak blocks before → after |
|---|---:|---:|---:|
| Small, 2,552 bytes | 386,790 → 389,366 (+2,576; +0.67%) | 211,874 → 212,898 (+1,024; +0.48%) | 200/91 → 200/91 |
| Medium, 32,429 bytes | 3,985,124 → 3,996,468 (+11,344; +0.28%) | 2,168,490 → 2,172,586 (+4,096; +0.19%) | 368/237 → 368/237 |
| Large, 260,429 bytes | 13,938,083 → 13,980,403 (+42,320; +0.30%) | 7,577,945 → 7,594,329 (+16,384; +0.22%) | 792/637 → 792/637 |
| Medium plus Unicode line, 40,623 bytes | 4,103,095 → 4,114,439 (+11,344; +0.28%) | 2,243,815 → 2,247,911 (+4,096; +0.18%) | 377/239 → 377/239 |

The legacy adapter has no snapshot-construction counterpart. The cold-build
Callgrind counts before and after adding parameter spans are:

| Snapshot input | Ir before → after | I1mr before → after | ILmr before → after |
|---|---:|---:|---:|
| Small, 2,552 bytes | 627,647 → 625,801 | 639 → 638 | 591 → 590 |
| Medium, 32,429 bytes | 6,569,209 → 6,567,392 | 634 → 623 | 580 → 576 |
| Large, 260,429 bytes | 27,527,697 → 27,535,063 | 638 → 622 | 581 → 577 |

These historical profiles used the persistent Linux/ARM64 Valgrind container.
New profiles run on hosted native CI with the examples' opt-in Rust DHAT allocator.
Different profiling backends/optimization modes are not equivalent baselines.

CI also collects `position-unicode`, `snapshot-small`, `snapshot-medium`,
`snapshot-large`, and `snapshot-medium-unicode`; inspect `Total` and
`At t-gmax` in DHAT output. The `.21` Callgrind table above gives direct
instruction/cache-event costs for conversion and cold snapshot construction.

## Bend 2 CLI fallback measurement

The installed compiler reports `bend 2.0.32`; `bend --help` exposes file
checking through `bend <file.bend> --check-only`. Upstream's
[Bend 2 limitations](https://github.com/bendlang/bend#limitations) explicitly
state “no incremental builds.” The [CLI entry point](https://github.com/bendlang/bend/blob/main/bend2/main.ts#L229-L301)
loads a `Book` for each `cli_file` check. Upstream does export lower-level
TypeScript functions such as [`book_load`](https://github.com/bendlang/bend/blob/main/bend2/bend.ts#L952-L952)
and `parse_book`, but does not document a stable incremental compiler service
or persistent-worker contract. The server therefore keeps the CLI fallback;
no daemon was added.

The ignored manual test
`tests/lsp_protocol.rs::measure_actual_bend_compiler_cache_fallback` wraps the
installed Bend executable, counts invocations, and forwards the staged graph
to the real CLI. The generated workspace has 98 shared dependency files and
one root-private file per root; each of the two roots reaches exactly 100
source files. One unrelated file is outside both graphs.

Setting `BEND2_LSP_COMPILER_METRICS_FILE` on the server appends one row per
compiler check: cache state, staged file/byte counts, staging duration,
`Command::output` child duration, and compiler-diagnostics total. Metrics-file
I/O is excluded from `total_ns`; unset, production-default operation performs
no metric-file I/O. Durations below were recorded on macOS 25.6/arm64 with
Bend 2.0.32. Per-check values are comma-separated in compiler completion
order. `child_ms` is the awaited configured command duration; in this
measurement the counting shell wrapper forwards to Bend, so it includes the
wrapper overhead. LSP wall time runs from the notification to matching
`publishDiagnostics` and includes the 250 ms debounce. The duplicate
same-revision row instead ends at the ordered hover response.

| Scenario | Bend process Δ | Checks | Staged files / bytes | Staging ms per check | Child ms per check | Compiler total ms per check | LSP wall ms |
|---|---:|---|---:|---:|---:|---:|---:|
| First root, 100-file graph | 1 | miss | 100 / 5,359 | 6.905 | 52.781 | 63.993 | 326.733 |
| Duplicate `didChange` at same revision | 0 | no check | 0 / 0 | — | — | — | 0.770 |
| Distinct second root | 1 | miss | 100 / 5,372 | 6.962 | 41.551 | 51.968 | 308.182 |
| Identical text, newer revision | 0 | hit | 0 / 0 | 0 | 0 | 0.110 | 254.839 |
| Unrelated file edit | 0 | 2 hits | 0 / 0 | 0, 0 | 0, 0 | 1.521, 1.480 | 268.458 |
| Shared dependency edit | 2 | 2 misses | 200 / 10,731 | 9.056, 9.417 | 40.967, 44.224 | 53.707, 58.001 | 317.887 |
| Compiler executable stamp change | 2 | 2 misses | 200 / 10,731 | 9.983, 10.764 | 51.416, 54.486 | 65.330, 69.574 | 323.941 |
| Compiler path/configuration change | 2 | 2 misses | 200 / 10,731 | 9.783, 10.368 | 57.965, 61.740 | 72.127, 76.440 | 328.253 |

The same-revision duplicate schedules no check; the newer-revision identical
text and unrelated-file scenarios each avoid Bend execution and staging on
cache hits. Dependency, compiler-stamp, and compiler-path invalidation each
stage both 100-file graphs (200 files and 10,731 aggregate source bytes).
Staging elapsed time includes `spawn_blocking` queueing and staging setup/copy;
compiler total includes semaphore wait, cache/stamp work, staging, child,
diagnostic parsing, and cache update. Timings are single-run local evidence,
not an SLA. The check asserts empty diagnostics for every real Bend result and
the expected per-check cache/file/byte/timing values; no sleeps are used.

`.19` fallback acceptance is evidenced by the measured process and staging
savings, cache hits, invalidation behavior, and empty diagnostics on both
100-file roots. Reproduce on a host with Bend 2 on `PATH`:

```sh
cargo test --locked --test lsp_protocol measure_actual_bend_compiler_cache_fallback -- --ignored --nocapture
```

The manual test is ignored during ordinary suites because it requires the
installed real Bend CLI.

## Folding scaling evidence

The 2026-09-29 Linux/ARM64 run used Rust 1.98.1, Valgrind 3.19.0, and exact
100/1,000/10,000-line sources without a trailing newline. The fixture is built
outside the measured query. Callgrind recorded:

| Lines | Instructions (Ir) | Ir / line | Scale from prior size |
|---:|---:|---:|---:|
| 100 | 13,194 | 131.94 | — |
| 1,000 | 121,694 | 121.69 | 9.22× for 10× input |
| 10,000 | 1,206,359 | 120.64 | 9.91× for 10× input |

The 100× input increase costs 91.4× instructions, consistent with O(lines).
The legacy-compatible folding values are from the historical pre-floor
comparison; their Iai gate used the former `Ir=2%`, `I1mr=3%`, and `ILmr=3%`
The fresh `.21` acceptance comparison used the active cache-event floor and
historically compared 25/25 workloads. That suite size is historical, not an
active fixed-count requirement.

Allocation counts came from Valgrind DHAT and are attributed to
`folding_ranges`, excluding snapshot construction. The scan counts nonempty
ranges, then allocates the returned `Vec<FoldingRange>` once at exact
capacity. Auxiliary vectors remain distinct from the returned result:

| Lines | Range vector allocations (bytes) | Active stack allocations (bytes) | Returned Vec allocations (bytes) | Total allocations in `folding_ranges` |
|---:|---:|---:|---:|---:|
| 100 | 1 (2,400) | 1 (64) | 1 (800) | 3 |
| 1,000 | 1 (24,000) | 1 (64) | 1 (8,000) | 3 |
| 10,000 | 1 (240,000) | 1 (64) | 1 (80,000) | 3 |

The returned vector still requires one allocation for nonempty output; exact
capacity removes its geometric growth reallocations. Total allocation count
stays constant at three per call, while the two auxiliary vectors are also
one allocation each. This meets the issue's no-growth-allocation criterion;
no acceptance limits changed. Current hosted native allocation profiles exercise
the same example at `100`, `1000`, and `10000` lines, both snapshot-only and folding.

Inspect only allocation stacks containing
`folding_ranges`. DHAT allocation events are separate from Callgrind
instruction events.

## Compact slots and indexed editor completion: PR #35 evidence

Hosted run [37815011502](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37815011502)
compared merge candidate `6e9484d92f2bf073509590c81635eeeac4a24390`
(head `f1ff844e4df8a2ad14e408e819dbbf1ed9265350`) with main
`8c866cb6b9d66ed1255b17086222fe5dba39a3e4`.
All 44 baseline instruction comparisons passed. Cold snapshot instructions
changed by -4.48%, -5.56%, and -1.93% for small, medium, and large fixtures.
Constructor completion changed by -19.64% (empty), -19.48% (prefix), and
-5.58% (subsequence); prewarmed constructor definition was unchanged.
Three new indexed import-candidate workloads cover exact, fuzzy, and absent names.

The compare job remains failed: 39/44 baseline workloads passed the combined
policy. Cache failures were completion/small I1mr 49→55 and ILmr 40→44;
inlay-hints/large I1mr 43→53; inlay-hints/medium 44→48;
scoped-completion/small I1mr 96→102; semantic-tokens/small I1mr 22→28.
These results do not establish the cause of the cache changes. Earlier failed
run 37809245614 and both runs' raw profiles are retained; neither is rerun to green.

Report-only latency measured completion p50 0.074→0.083 ms and p95
0.089→0.096 ms; hover during a large open p50 0.693→1.079 ms and p95
0.800→1.162 ms. Large open/edit p50 improved by 1.89%/0.97%.
The isolated, source-identical compact-index memory comparison in the README
measured 26.8% less peak live heap and 19.6% less allocation traffic; it does
not isolate the later editor completion features. Quality passed locally
(184 tests, one skip) and in hosted run 37815011492.
Instruction, cache, baseline, retry, and benchmark policies remain unchanged.

