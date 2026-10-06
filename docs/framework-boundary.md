# Framework boundary and dispatch decision

Research spike: `bend2-lsp-rs-fzo.8`, reviewed 2026-10-04 against the server boundary on main `1e75117a8d5be38d648f65f25d854c894cffbafd`. This is a decision record, not a framework migration or a performance result. Upstream versions and concurrency claims below come from registry metadata and released source, not the initiating review.

## Decision

Retain `tower-lsp 0.20` for this orchestration change. Keep document/workspace revision coordination independent of framework scheduling. Treat `tower-lsp-server` as the preferred candidate **if a separate migration is justified** by a needed protocol feature, a demonstrated library defect, or dependency-maintenance requirements. Do not select a prerelease implicitly, change concurrency to one as a correctness fix, or replace the framework with manual dispatch in this epic.

The fork has observable maintenance advantages, but neither its stable release nor its current prerelease serializes document mutations and dependent feature queries to completion. Migration cannot replace the revision protocol. No comparative latency, allocation, throughput, or executable-size improvement has been measured by this spike.

## Explicit boundary

The existing division is worth preserving:

- [`analysis`](../src/analysis.rs) owns immutable `DocumentSnapshot`, byte `TextRange`, `LineIndex`, compact syntax/occurrence indexes and synchronous indexed queries. It does not import a framework or LSP DTOs. No framework migration should add async execution or warm full-source semantic scans here.
- [`workspace`](../src/workspace.rs) owns file identity, immutable snapshot storage and the external occurrence reverse index used by references and rename. Call hierarchy retains snapshot-based incoming traversal and local outgoing traversal; no workspace call-edge index is stored or built. It currently uses `url::Url` as an application URI key, not an LSP `Position`, `Location`, `Diagnostic`, or request type. That is an explicit identity choice, not proof that any replacement wire URI type has identical normalization/equality semantics.
- [`server/adapters.rs`](../src/server/adapters.rs) and [`server/features.rs`](../src/server/features.rs) convert indexed byte ranges and semantic results into protocol DTOs. [`server/capabilities.rs`](../src/server/capabilities.rs) owns advertised wire capabilities. This is the selected DTO boundary; it is not confined to a single file.
- The server orchestration side owns typed request handling, revision capture/admission/commit barriers, task scheduling and diagnostics publication. [`server/compiler.rs`](../src/server/compiler.rs) currently returns LSP diagnostics and is inside that server-side boundary, not part of the transport-independent analysis core.
- [`server/transport.rs`](../src/server/transport.rs) owns stdio wrappers, the outer Tower service, framework framing/dispatch integration, process termination and final compiler/task cleanup. Framework cancellation is not a substitute for the application's task ownership, compiler kill/reap, or shutdown drain.

Future adapters may convert a wire `Uri` to the existing workspace identity at ingress and back at egress. Do not spread fork DTOs into analysis or change workspace identity merely to make imports compile. Preserve unsaved buffers, generated Base source provenance, UTF-16 conversions, existing capabilities and compiler configuration semantics.

## Verified release and compatibility comparison

The authoritative [original registry index][original-index] ends at `0.20.0`, published 2023-08-11. The [fork registry index][fork-index] lists stable `0.23.0`, published 2025-12-07, and newer prerelease `0.24.0-rc.1`, published 2026-09-11. The prerelease is not the latest stable release. Dependency requirements below are semver ranges, not a prediction of a future lockfile.

| Boundary | Existing original `0.20.0` | Fork stable `0.23.0` | Fork prerelease `0.24.0-rc.1` |
| --- | --- | --- | --- |
| Protocol types | `lsp-types ^0.94.1`, reexported as `tower_lsp::lsp_types` | `ls-types ^0.0`, reexported as `tower_lsp_server::ls_types` | `gen-lsp-types ^0.11`; new generated types, not the stable fork's types |
| URI representation | LSP DTOs use `url::Url` | `ls_types::Uri`, with file-path conversion methods | Configurable URL backend: `url-url` / `url-fluent-uri`; default generated URI representation |
| Tower requirement | `^0.4`, `util` | `^0.5`, `util` | `^0.5`, `util` |
| LanguageServer implementation | `#[tower_lsp::async_trait]`, boxed async trait machinery | Return-position `impl Future + Send`; remove `async_trait`; trait is not dyn-compatible | Same native trait style, with generated protocol API changes |
| Manifest Rust requirement | 1.64.0, edition 2021 | 1.85, edition 2024 | 1.87, edition 2024 |
| Runtime features | Default Tokio; optional runtime-agnostic codec | Default Tokio; optional runtime-agnostic codec | Same runtime choice; URI feature choices replace the old `proposed` feature |

Sources: tagged [original manifest][original-manifest], [stable fork manifest][fork-manifest], [prerelease manifest][rc-manifest], [stable reexports][fork-lib], [fork migration changelog][fork-changelog], [prerelease changelog][rc-changelog], and [stable URI API][fork-uri]. The current repository lockfile resolves `tower-lsp 0.20.0`, `tower 0.4.13`, `lsp-types 0.94.1`, and `url 2.5.8`; its manifest pins a toolchain newer than all three framework minimums. That does not establish cross-target support for a different dependency graph.

**Tower nuance:** a major-version change in `tower` is not, on its own, a different `Service` trait. [Tower 0.4.13][tower04-lib] and [Tower 0.5.2][tower05-lib] both reexport `tower_service::Service`; their manifests depend on compatible `tower-service ^0.3.1` and `^0.3.3` respectively ([0.4 manifest][tower04-manifest], [0.5 manifest][tower05-manifest]). The current outer service is therefore not inherently incompatible at the trait boundary. Framework-specific `jsonrpc::Request`/`Response` types still differ, and middleware APIs/features must be checked. A migration should align the direct Tower dependency deliberately rather than retain two versions accidentally.

A stable-fork migration is more than a crate rename: change all server DTO imports, async-trait usage, URI conversion points and response constructors. The fork corrects `workspace/symbol` to use `WorkspaceSymbolResponse`; that handler cannot simply keep its old signature. The prerelease changes the types package again and adds LSP 3.18 methods; its `url-url` option may reduce URI conversion work but does not establish drop-in DTO compatibility. Update protocol contracts and capabilities intentionally, not by accepting new defaults.

## Dispatch, cancellation and shutdown

Both tagged transports, and the current prerelease transport, have these source-visible properties ([original][original-transport], [stable fork][fork-transport], [prerelease][rc-transport]):

1. Frames are decoded sequentially. Each incoming request/notification awaits `poll_ready`, then invokes `Service::call` in ingress order. This **admission order** is distinct from executing a typed async handler.
2. Returned futures enter a bounded channel (`MESSAGE_QUEUE_SIZE = 100`) and are driven through `buffer_unordered(max_concurrency)`. Default maximum concurrency is **four**, not one, unbounded spawning, or a Tokio-worker count. The futures need not complete in ingress order; four is a future limit, not a bound on backend-spawned tasks or compiler jobs.
3. Initialization has additional readiness/state gating. That special-case lifecycle ordering is not a general document-update barrier.
4. `concurrency_level(1)` forces sequential future processing and is explicitly documented to implicitly disable `$/cancelRequest`. Queue pressure and readiness can also delay admission. There is no evidence that increasing the limit fixes causal document ordering or improves this application's latency.

The [LSP ordering specification][lsp-order] allows independent responses to be reordered only when correctness is unaffected. It specifically warns about reordering document changes with dependent requests. Reserve/capture causal revisions at the application's ingress boundary and wait for the relevant committed indexed view; retain this protocol for either library.

Both libraries track request IDs and use `AbortHandle`/abortable futures; `$/cancelRequest` aborts the tracked future and produces `RequestCancelled` (`-32800`). Notifications do not become cancellable requests merely because they share the transport. Dropping a handler future does not automatically abort a `tokio::spawn` task, undo an already committed snapshot, stop a blocking computation, or kill/reap its compiler child. The application must retain those ownership contracts. See [original pending requests][original-pending], [fork pending requests][fork-pending] and their [original][original-layers]/[fork][fork-layers] middleware. The [cancellation specification][lsp-cancel] still requires a response for a cancelled request.

At framework `shutdown`, middleware changes state to `ShutDown` and invokes the backend handler; it does **not** itself call `pending.cancel_all`. At `exit`, middleware sets `Exited`, aborts tracked requests and closes the client channel. In original `0.20.0`, the transport read loop can continue waiting for stdin after `exit`; the application's outer exit-aware service already handles that. In fork `0.23.0` and the prerelease, the read loop explicitly breaks after dispatching `exit`, disconnects channels and aborts the client-request stream. The fork changelog's “1s after exit” language is **not a production one-second timer**: the tagged implementation has an immediate read-loop break and a one-second test timeout. Neither behavior proves that all application-owned jobs are drained.

Therefore retain the existing outer exit/EOF/SIGTERM lifecycle and task/compiler cleanup until an independently verified migration deliberately replaces only the redundant transport part. Do not remove kill/reap, trace flush, shutdown drain or generated-source lifetime ownership because a framework returns from `serve`.

## Maintenance, security and portability

- Original `0.20.0` has no later registry release as of this review. That is a concrete release-staleness signal, not a claim that it is unusable or has a particular vulnerability.
- The fork has later stable releases and a recent prerelease. Its tagged changelog documents fixes for a cancelled server-to-client response panic, null response handling, transport exit/hanging behavior, URI escaping/Windows conversion and invalid/empty URI panics. These are meaningful correctness/robustness signals; a changelog is not proof that every dependency or malformed frame is safe.
- A future migration must run the unchanged [`deny.toml`](../deny.toml) policy on the actual resolved graph, retaining advisory/yank/license/source checks and no new ignores. No full dependency security audit was executed during this research. Absence of an advisory result or a public advisory page is not a security guarantee; do not classify these correctness fixes as CVEs without an advisory.
- Both frameworks expose Tokio and runtime-agnostic I/O features and avoid a mandatory executor spawn in their transport concurrency mechanism. Their generic I/O permits transports other than stdio. This does not make this Bend server WASM-portable: it uses filesystem access, compiler subprocesses, stdio threads and native termination handling. Keep all existing native platform/release gates. URI handling needs explicit Unix/Windows file-path, percent-encoding and non-file-URI checks if changed.

## Alternatives and migration cost

| Choice | Work and risk | Decision |
| --- | --- | --- |
| Keep original behind current adapters | Preserve existing public protocol and revision/lifecycle behavior; carry stale-release risk and monitor dependency policy | Selected for this epic |
| Stable fork `0.23.0` | Server-wide DTO/URI and trait migration; workspace-symbol response update; test harness/transport type changes; lockfile/license review and all native/protocol gates | Best candidate for a separately justified migration; no assumed ordering or speed win |
| Prerelease `0.24.0-rc.1` | Above, plus another generated-types/capability transition and explicit URI backend selection | Observe; do not silently promote into production |
| Manual dispatch | Own framing and malformed-input handling, JSON-RPC IDs/errors, typed routing, initialization state, cancellation, backpressure, outbound client requests, response correlation, capability evolution and exit/EOF behavior | Highest protocol maintenance cost; no demonstrated benefit justifies replacing tested library machinery |

Migration acceptance would require unchanged semantic/revision tests and real stdio proof, compiler cancellation/reaping on supersession/close/shutdown/EOF/SIGTERM, Base provenance/navigation, native URI behavior, unchanged dependency policy, and an equivalent paired latency experiment. It is a separate decision, not a prerequisite for the current orchestration services.

## Bounded parent-run experiment

**Execution status:** the parent exercised the immutable release baseline and integrated orchestration candidate. The integration evidence below separates protocol correctness, local latency, and the failed local Callgrind comparison. No fork executable was built or measured; this remains a decision to retain the current framework.

### Observed immutable release baseline

The parent ran the same-write protocol scenario against `/tmp/bend2-stable-release-025/target/release/bend2-lsp` (SHA-256 `fffede1f5482c296c6ef4d0c4d581f08bd7e23a2d0cfcaba5a2f2ced5d4d93d1`). All sixteen changes, versions 2–17, produced hovers naming the latest revision, and shutdown completed cleanly. This is the parent's observed semantic/lifecycle result; its saved `protocol.json` records revision/request IDs and timings, rather than full hover payloads.

Read-only trace inspection confirmed that each hover request ID 3–18 was admitted after its immediately preceding `other` service call. The first hover future's recorded lifetime overlaps the first `did_change` handler lifetime, demonstrating why ingress admission must not be confused with serialized handler completion. Notifications are categorized as `other`, so the trace does not independently assign them request IDs.

Raw local evidence: `/var/folders/0r/pkz80d0154z37ybnv69pqclh0000gn/T/bend-framework-baseline-rwh1i2o6/protocol.json` and the sibling `trace.json`. Preserve/archive this temporary evidence before cleanup. Tracing was enabled; recorded durations are **not** a comparative performance result. This proves the immutable release baseline scenario only, not the unexercised candidate or a framework migration.

### Protocol order and causal revision observation

From the repository root, after the parent's single integrated build, run the following throwaway Python process. It reuses the existing executable [stdio latency client](../scripts/lsp_latency.py), not private Rust internals. Sixteen `didChange` + dependent hover pairs are each written as one concatenated byte sequence, with no intervening readiness wait. Each hover must name the new revision, then the next pair starts. A settled initial open isolates change ordering from workspace initialization. The compiler path and home are isolated by the existing harness. Each client operation has a ten-second deadline; the run never retries failures. Raw per-revision results and a Chrome trace are retained in a fresh temporary directory on both success and failure.

```sh
python3 - ./target/debug/bend2-lsp <<'PY'
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import time

spec = importlib.util.spec_from_file_location("latency", "scripts/lsp_latency.py")
h = importlib.util.module_from_spec(spec)
spec.loader.exec_module(h)
h.REQUEST_TIMEOUT = 10.0
# Keep trace enabled in this throwaway client; production harness is unchanged.
h.REMOVED_ENVIRONMENT = tuple(k for k in h.REMOVED_ENVIRONMENT if k != "BEND2_LSP_TRACE")
binary = Path(sys.argv[1]).resolve(strict=True)
evidence = Path(tempfile.mkdtemp(prefix="bend-framework-spike-"))
workspace = evidence / "workspace"
workspace.mkdir()
trace = evidence / "trace.json"
os.environ["BEND2_LSP_TRACE"] = str(trace)
body = Path("benches/fixtures/analyzer_large.bend").read_text()
body = (body + "\n") * 4
uri = (workspace / "dispatch.bend").as_uri()
rows = []
report = {"binary": str(binary), "sha256": h.sha256_file(binary), "rows": rows,
          "completed": False, "trace": str(trace)}
print(f"evidence: {evidence}", flush=True)
client = h.LspProcess(binary, workspace)
try:
    client.initialize()
    name, source = h.large_revision(body, "dispatch", 1)
    client.notify("textDocument/didOpen", h.open_message(uri, source)["params"])
    initial, _ = client.request("textDocument/hover", h.position(uri, 0, 5))
    h.require_hover(initial, f"def {name}: U32")
    client.wait_diagnostics(uri, 1)
    for revision in range(2, 18):
        previous = name
        name, source = h.large_revision(body, "dispatch", revision)
        request_id, method, request = client.prepare_request(
            "textDocument/hover", h.position(uri, 0, 5))
        client.pending[request_id] = method
        started = time.perf_counter_ns()
        client.write_frame(h.frame(h.change_message(uri, source, revision)) + request)
        result, elapsed = client.response(request_id, started)
        rows.append({"revision": revision, "request_id": request_id,
                     "elapsed_ns": elapsed, "result": result})
        h.require_hover(result, f"def {name}: U32", previous)
    client.finish()  # null shutdown response, exit, bounded process exit, trailing framing
    report["completed"] = True
except BaseException as error:
    report["error"] = f"{type(error).__name__}: {error}"
    report["stderr"] = client.stderr_tail()
    raise
finally:
    client.close()
    (evidence / "protocol.json").write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps({"completed": True, "revisions": len(rows), "evidence": str(evidence)}))
PY
```

Inspect `trace.json` using the [existing tracing guide](tracing.md). `transport.service_call` records ingress admission; `transport.service_future` records a future's lifetime; `document.update` and revision/commit spans record backend progress. Correlate numeric request IDs from `protocol.json` with the service spans. The notification currently has transport category `other`, so correlate the sole preceding update using its revision and surrounding spans rather than inventing a notification ID. A single client write is not a guarantee of one OS read or one framework batch.

Expected evidence is ingress admission of the change before the matching hover, and hover data from that changed committed revision. A success is application-level causal behavior, **not** proof of serialized framework futures. A failure must retain the old/new names, request ID, trace and stderr; do not add sleeps or retries to erase it. The trace may be incomplete after failure cleanup kills the process. Latencies from this trace-enabled observation are diagnostic only, not comparable to the untraced latency harness below.

For a future framework-only comparison, use a separate throwaway executable for each exact release and a `Service::call` recorder around its `LspService`. Register a custom request whose handler signals “started” and waits on a gate. Send slow request A, then independent immediate request B; require the recorder to show A/B ingress order and B to respond before releasing A. Repeat with A followed by `$/cancelRequest`, requiring `-32800` and a handler drop signal. Use five-second gates/timeouts, no sleeps as synchronization, and shutdown response followed by exit with stdin deliberately left open. Record the original transport's open-stdin exit limit separately from the application's outer exit wrapper. This isolates framework future concurrency/cancellation from Bend's snapshot work; it is not a new permanent test seam or a production dispatcher.

### Paired latency and protocol smoke

Use the existing harness with the **same binary on both sides** for a small A/A smoke. It validates hover/definition/completion semantics, changed-revision hovers, unrelated pipelined hovers during large opens/edits, framing, shutdown and process exit. It checks binary/harness/fixture identities and stores raw nanosecond samples. It intentionally disables tracing and real compiler execution.

```sh
python3 scripts/lsp_latency.py \
  --baseline-binary ./target/debug/bend2-lsp \
  --candidate-binary ./target/debug/bend2-lsp \
  --baseline-output /tmp/framework-aa-baseline.json \
  --candidate-output /tmp/framework-aa-candidate.json \
  --rounds 2 --samples 4 --warmup 1
```

Choose fresh output paths to preserve failed/prior evidence. For a separately approved migration, substitute independently built baseline/candidate release binaries from pinned worktrees, record source identities, and use the same harness/fixture/configuration with the normal larger paired run. Warm-request and causal open/edit latency are different measurements; compiler execution, initialization and trace overhead are not included here. Do not report the A/A smoke as a performance gate, infer tail percentiles from four samples, or claim that protocol latency establishes cold/warm Callgrind equivalence. Retain unchanged [performance policy](performance-policy.md) gates.

## Observed orchestration integration

The integrated candidate passed `./scripts/quality`: 110 Rust tests passed, one
skipped; dependency, policy, workflow-security and existing Python contract gates
passed. Release stdio smoke exercised unopened importer references/rename/incoming
calls, outgoing calls from an opened caller, ignored-directory exclusion, watched
deletion/recreation with current ranges, and clean shutdown. With actual Bend
2.0.34, `IO.print` navigated to compiler-owned Base; opening that source produced
zero diagnostics, retained hover, and exited cleanly.

The immutable release baseline SHA-256 was
`fffede1f5482c296c6ef4d0c4d581f08bd7e23a2d0cfcaba5a2f2ced5d4d93d1`;
the integrated release candidate SHA-256 was
`5033796d1991a1102529a5d7385fab67a9651f29ed67a56961f4a2a0465155b5`.
The normal seven alternating latency rounds, 32 samples and eight warmups per
workload completed. Local macOS ARM p50 milliseconds:

| Workload | Baseline | Candidate |
| --- | ---: | ---: |
| Warm hover | 0.048 | 0.058 |
| Warm definition | 0.046 | 0.046 |
| Warm completion | 0.047 | 0.044 |
| Large open to hover | 15.064 | 21.949 |
| Large edit to hover | 16.214 | 25.688 |
| Hover during large edit | 0.617 | 0.686 |
| Hover during large open | 0.600 | 0.660 |

An additional causal batch smoke used a 2,604,400-byte source, five samples per
case, and demanded the final revision's declaration from each hover. Median
milliseconds for baseline/candidate were 69.362/95.764 for one full replacement,
483.744/580.679 for eight, and 1909.360/2295.631 for 32. This bounded sequential
experiment is not an alternating statistical gate or evidence for a Rope/parser
replacement. Preserve full replacement semantics; no incremental representation
change was made.

The first persistent Linux ARM Callgrind comparison **failed unchanged gates**:
28/36 existing workloads passed, with 30 additional cold/warm semantic-index
workloads measured. Initial workspace instructions increased
37,498,399 to 59,143,363; burst invalidation increased 5,479,990 to 8,922,668;
the existing workspace-references workload increased 2,010,818 to 3,091,345.
Cache failures also affected folding, medium semantic tokens and incremental
invalidation. No thresholds, baseline selection, metrics or failed results were
changed, and this local ARM result does not substitute for hosted x86 CI.

New sparse warm query workloads kept three matching importers while increasing
the workspace from 100 to 1,000 to 10,000 files. Reference instructions were
9,952/9,091/9,400; incoming-call instructions were 11,253/11,241/12,160;
outgoing-call instructions were 7,074/6,958/7,861. These are standalone indexed
query measurements, not complete editor latency or a before/after speedup.
The cold update workload owns and returns its whole fixture; its profile includes
destruction. At 10,000 sparse files, allocator free/consolidation functions
dominated its 60,277,016 instructions. Do not interpret that total as isolated
single-file commit work, or subtract destruction to turn a failed gate green.

Local raw evidence is preserved under `/tmp/bend2-architecture-callgrind`,
`/tmp/bend2-architecture-latency-{baseline,candidate}.json`,
`/tmp/bend2-architecture-latency-report.{json,md}`, and
`/tmp/bend2-architecture-batch-proof.json`. Tool transcripts retain the quality
and actual-compiler smoke output. Performance retention requires maintainer
acceptance of this specific tradeoff or further optimization and fresh evidence;
these results do not authorize merging or releasing the candidate.

### Cold-index optimization evidence

Three subsequent implementations removed duplicate local references/calls from
workspace maps, reused immutable syntax indexes, introduced dense reachability
membership, and shared compact external-target ordinals between references and
calls. External call targets are stored in a lazily allocated snapshot-ordinal
column; all-local files do not allocate that column.

The third implementation passed `./scripts/quality`: 111 Rust tests passed and
one existing test was skipped. Its persistent Linux ARM comparison still
**failed unchanged gates**: 26/36 existing workloads passed; 30 new workloads
were measured. Initial workspace instructions were 45,403,975 versus the
37,498,399 baseline (+21.08%), burst instructions were 6,160,171 versus
5,479,990 (+12.41%), and workspace-reference instructions were 2,576,671 versus
2,010,818 (+28.14%). Cold snapshot, folding, identifier-range and incremental
workloads also had instruction/cache failures. Reducing the initial candidate's
cost is not equivalent to passing the baseline gates.

The second implementation's seven-round paired macOS ARM run measured large
open p50 16.255/16.238 ms and large edit p50 17.835/17.841 ms
(baseline/candidate), but edit p95 was 33.174/42.278 ms. Compilation and
Callgrind ran concurrently, so these results do not establish an isolated
latency improvement or complete recovery. They do not measure the third
implementation's release executable.

Preserved profiles:
`/tmp/bend2-architecture-callgrind-optimized-{1,2,3}`.
Second-implementation paired samples and reports:
`/tmp/bend2-architecture-latency-optimized-2-{baseline,candidate}.json` and
`/tmp/bend2-architecture-latency-optimized-2-report.{json,md}`.
No failed run was discarded, no performance policy was changed, and no
maintainer approval to retain the remaining regression has been obtained.

The third implementation's release executable SHA-256 is
`d8a84fe7494ab324518e52a2de61c3e863fc4d5feb1484c093612194488e31a1`.
Its actual stdio smoke verified two reference locations across an opened
definition and unopened importer, cross-file rename, incoming/outgoing calls,
ignored-directory exclusion, and clean shutdown. With actual Bend 2.0.34,
definition navigation reached generated Base and opening it produced zero
diagnostics before clean shutdown.

The third release completed seven alternating latency rounds with 32 samples
and eight warmups per workload, after builds and Callgrind completed. Large
open p50 was 23.880/23.509 ms and large edit p50 was 24.995/24.867 ms
(baseline/candidate). Edit p95 remained worse: 54.083/61.155 ms (+13.08%).
Warm hover p50 was 0.042/0.045 ms; definition 0.044/0.043 ms; completion
0.039/0.042 ms. Hover during a large edit was 1.437/1.607 ms p50 (+11.83%).
These local report-only measurements do not override failed Callgrind gates.
Raw samples and reports are preserved in
`/tmp/bend2-architecture-latency-optimized-3-{baseline,candidate}.json` and
`/tmp/bend2-architecture-latency-optimized-3-report.{json,md}`.

The fourth implementation made external source groups canonical, removing
duplicate incoming/outgoing range maps and Arc allocations; source contributions
use one dense FileId column while external target/source buckets remain sparse.
Cold syntax construction reuses existing qualifier/caller metadata, a call
cursor, and grouping scratch storage. `./scripts/quality` passed again:
111 Rust tests passed, one existing skip.

Initial workspace instructions reached 37,461,715 versus baseline 37,498,399;
burst instructions reached 5,029,537 versus 5,479,990. Both instruction gates
passed. Overall unchanged local Linux ARM gates still **failed** (27/36):
initial I1mr 99,706/80,092 and ILmr 1,136/902; burst I1mr 9,461/9,055
and ILmr 171/147; references Ir 2,180,843/2,010,818 and ILmr 157/123
(candidate/baseline). Incremental instructions, folding cache, small identifier
ranges, and large semantic-token cache also failed. Raw profiles are preserved
at `/tmp/bend2-architecture-callgrind-optimized-4`.

Fourth release SHA-256:
`1842b503bab8a317fc11b899308ded55ba07f7f3185b16083aa75f58de723b15`.
Actual stdio smoke verified two aliases to one imported function merge into one
incoming/outgoing group with two ranges; replacing the caller snapshot removes
the obsolete range, and references shrink from three to two exact locations.
Shutdown completed cleanly. No fourth-release comparative latency result is
claimed here. The maintainer explicitly selected continued optimization again;
remaining regressions are not approved for retention.

The fifth trial replaced nested external caller-range allocations with one
per-source array of snapshot call ordinals and grouped spans. It also shared
grouping scratch storage across cold syntax phases and experimented with
non-inline call/reference construction boundaries. Pre-lint-fix profiles are
retained at `/tmp/bend2-architecture-callgrind-optimized-5-prelint`; unchanged
gates failed overall (24/36 passed). References instructions decreased to 2,062,980 versus
2,010,818 baseline, but initial I1mr rose to 101,556 and burst I1mr to 9,936.
The non-inline boundary experiment was removed rather than retained as an
unproven cache improvement. Snapshot ordinals and scratch reuse remain subject
to fresh integrated gates.

After explicit preparation-return and symbol-ID-assignment refactors corrected
two Clippy failures, quality passed (111 Rust tests, one existing skip).
Release SHA-256
`2eb3d25988002a789844da4cd25aedf58ab2745753dc2294c8148644106f29fc`
passed actual stdio smoke for interleaved callees, two aliases merging into one
two-range group, and replacement removing stale caller/callee groups before
clean shutdown. This binary predates removing the non-inline experiment;
it is not the final retained optimization's runtime proof.

After removing the non-inline experiment, the retained fifth candidate passed
quality (111 tests, one existing skip) and release stdio interleaved-target,
alias-merge, replacement, and shutdown smoke. Release SHA-256:
`de5466a8cc499f98495a04be07d4b29fe79f3841933f2452d6df16c0527ff0d0`.
Preserved profiles: `/tmp/bend2-architecture-callgrind-optimized-5`.
The unchanged comparator failed overall (26/36 passed): references Ir
2,063,191 versus 2,010,818; incremental Ir 1,074,655 versus 1,049,352;
initial I1mr 98,552 versus 80,092 and ILmr 1,133 versus 902; burst I1mr
9,673 versus 9,055 and ILmr 164 versus 147. Warm cache failures remained.
The instruction/cache failures were preserved, not rerun to select a green
result.

A sixth trial stored external occurrence TokenIds rather than copied
range/kind payloads. Quality passed (111 tests, one existing skip), but unchanged
Callgrind gates failed overall (25/36 passed): references Ir 2,064,308 versus
2,010,818 and incremental Ir 1,075,120 versus 1,049,352; initial I1mr was
99,065 versus 80,092 and ILmr 1,138 versus 902. Its profiles remain at
`/tmp/bend2-architecture-callgrind-optimized-6`. The trial did not establish the
intended instruction improvement and was removed; no runtime proof for that
discarded release is claimed.

The seventh implementation removed the additional outgoing caller/link arrays:
outgoing queries reuse immutable `calls_from` spans and the prepared external
call-target column. Incoming groups keep compact snapshot call ordinals.
Quality passed (111 tests, one existing skip); release
`15a281625ec4b6f69841b753d117c188fdd9efdaa9c264250107761574c7c1c6`
passed actual interleaved-callee, alias-merge, replacement, and shutdown smoke.
Profiles remain at `/tmp/bend2-architecture-callgrind-optimized-7`.

All four workspace instruction gates passed: initial Ir 36,765,670 versus
37,498,399, burst 4,861,988 versus 5,479,990, references 1,497,083 versus
2,010,818, and incremental 1,069,471 versus 1,049,352. Overall gates still
failed (22/36 passed), including initial I1mr 99,384/80,092 and ILmr
1,111/902, burst I1mr 9,499/9,055 and ILmr 156/147, and several warm
instruction/cache workloads. More instruction improvement does not establish
cache-policy equivalence.

The eighth implementation keeps the first name candidate inline in its hash
bucket and allocates a spill vector only for fingerprint collisions; source-text
collision comparisons and NameId ordering remain unchanged. Quality passed
(111 tests, one existing skip). Release
`4b66fbe657cdb7d2d8272edadce75ee0c649b9d77ed0faa3baa9ebaf0f923371`
passed actual alias/interleaved-call/replacement smoke and actual Bend 2.0.34
generated Base navigation/open with zero diagnostics and clean shutdown.
Profiles remain at `/tmp/bend2-architecture-callgrind-optimized-8`.

Workspace Ir candidate/baseline: initial 36,012,307/37,498,399;
burst 4,430,666/5,479,990; references 1,654,578/2,010,818; incremental
741,156/1,049,352. All four instruction gates passed. Large cold snapshot
Ir was 26,664,644/27,554,558, with I1mr 610/627 and ILmr 570/578.
Overall gates still failed (22/36 passed), including initial I1mr
98,284/80,092 and ILmr 1,107/902, medium identifier Ir 727/690 and
small semantic-token Ir 20,528/20,107. Warm and workspace cache failures
remain unapproved; the reduced allocation cost is not a complete gate pass.

The ninth implementation replaces the symbol-name HashMap with a dense
NameId-indexed immutable column, preserving first-declaration precedence.
Quality passed (111 tests, one existing skip). Release
`13164489ca1143a70433fc22f444e7e050c36fa8a295cb0f2ce7a6ef4cbd3ecc`
passed actual stdio references, alias-merged incoming/outgoing calls,
interleaved targets, replacement, and clean shutdown.
Raw profiles remain at `/tmp/bend2-architecture-callgrind-optimized-9`.
Unchanged gates passed 29/36 original workloads. Remaining failures:
folding ILmr 27/22, 26/20, and 23/18 (large/medium/small);
small semantic-token Ir 20,528/20,107; burst I1mr 9,735/9,055 and
ILmr 155/147; initial I1mr 99,225/80,092 and ILmr 1,112/902;
references I1mr 288/276 and ILmr 152/123. These local ARM Linux
measurements do not establish hosted x86_64 CI parity or authorize retention
of the remaining regressions.

The tenth trial caches the semantic-token result count during cold construction
and reserves the exact warm result capacity. This avoids retaining a duplicate
classification array or overallocating for punctuation-only input.
All original instruction gates passed. Small/medium/large semantic-token Ir
was 17,141/20,107, 166,754/177,882, and 498,291/526,573.
Initial/burst/references/incremental workspace Ir was
35,341,257/37,498,399, 4,386,161/5,479,990,
1,658,774/2,010,818, and 742,444/1,049,352.
Overall unchanged gates still failed (29/36 passed): small completion I1mr
32/27; folding ILmr 27/22, 26/20, 23/18; burst I1mr 9,938/9,055 and
ILmr 165/147; initial I1mr 99,630/80,092 and ILmr 1,128/902;
references ILmr 153/123.
Raw profiles remain at `/tmp/bend2-architecture-callgrind-optimized-10`.
This trial preceded the capacity helper extraction needed by the unchanged
Clippy function-length gate and is not the retained candidate's verification.

The eleventh implementation extracts cold result counting into a helper and
uses `trim_start().is_empty()` for folding's blank-line predicate. Both trimming
forms are empty exactly for whitespace-only input. Quality passed (111 tests,
one existing skip). Release
`036b3f2c7c845c9886c18140ab2c2710dcecb8d7ff25e5a3bf43909a6c3bbc24`
matched the immutable 0.2.5 release's semantic-token and folding responses
exactly across mixed token kinds, Unicode blank lines, punctuation-only input,
nested folds, trailing whitespace, and replacement to empty; shutdown was clean.
Folding Ir candidate/baseline was 11,188/13,194 (100 lines),
101,686/121,694 (1,000), and 1,006,285/1,206,359 (10,000).
All original instruction gates passed. Overall cache-inclusive gates still
failed (29/36 passed): small completion I1mr 34/27; folding ILmr
27/22, 26/20, 23/18; burst I1mr 9,936/9,055 and ILmr 165/147;
initial I1mr 99,430/80,092 and ILmr 1,128/902; references ILmr 153/123.
Raw profiles remain at `/tmp/bend2-architecture-callgrind-optimized-11`.
These results neither authorize retaining the remaining regressions nor cover
subsequent lifecycle corrections. Independent source review found stale active
Base resolution after failed compiler reload and a deleted imported disk
snapshot retained after rediscovery; both were reproduced through this release's
public LSP before repair.

Lifecycle review corrections reset active Base binding, compiler configuration,
and cached Base state in one existing workspace commit while holding the
Base-load mutex and update barrier. Old generated document ownership/provenance
remains intact. Rediscovery captures cached disk paths only in active roots,
probes omitted paths during cold staging, and prepares tombstones only for
explicit `NotFound`; ignore/symlink omissions or other filesystem errors do not
prove deletion. Generation-validated commit uses the existing disk update API,
preserving open overlay precedence and stable FileId.

Five public protocol regressions passed: failed/empty Base reload and recovery,
old URI lifetime, unwatched deletion/recreation, open overlay preservation,
ignored explicit imports, and removed-root scope. Separate actual debug stdio
smokes confirmed both failing-before/passing-after transitions and clean
shutdown; corrected debug binary SHA-256 was
`995f570a52e32c8a4ba80acd1ca884f2547885267fb6f2d47972b5e2cc973a71`.
The eleventh candidate's performance and release proof predate these corrections.

The twelfth candidate includes both lifecycle repairs. Required quality passed:
116 tests, one existing skip, unchanged lint/security/dependency gates. The
permanent Base and unwatched-deletion regressions also failed against preserved
pre-fix release `036b3f2c7c845c9886c18140ab2c2710dcecb8d7ff25e5a3bf43909a6c3bbc24`
at the actual stale hierarchy and stale declaration assertions.
Corrected release SHA-256
`165f9c6f88cf8905ad63e4b5b29cea4b747b801e007a84eebbad2de71843fd11`
passed separate stdio semantic/folding equivalence and both lifecycle transition
smokes, including deletion/recreation, overlay precedence through close, active
Base target detachment/rebinding, retained old URI backing, and clean shutdown.
Actual Bend 2.0.34 verified IO.print navigation/hover, generated Base with zero
diagnostics, failed reload removing navigation, successful reload recovery, and
old URI readability. That actual-compiler smoke does not claim namespace call
hierarchy support: plain IO.print outgoing calls were empty in both immutable
0.2.5 and corrected releases; indexed Base hierarchy reset was exercised with
the controlled compiler fixture.

All 36 original instruction gates passed. Candidate/baseline workspace Ir:
initial 35,365,248/37,498,399 (-5.69%), burst 4,384,766/5,479,990 (-19.99%),
existing references fixture 1,658,920/2,010,818 (-17.50%), and incremental
742,447/1,049,352 (-29.25%). The existing references fixture includes destruction;
it is not isolated editor-reference latency. Large cold snapshot Ir was
25,962,738/27,554,558 (-5.78%). Small semantic-token and large folding Ir remained
17,141/20,107 and 1,006,285/1,206,359.
Overall unchanged cache-inclusive gates failed (27/36 passed): small completion
I1mr 31/27; folding ILmr 27/22, 26/20, 23/18; small identifier I1mr 12/8;
small inlay ILmr 65/61; burst I1mr 9,773/9,055 and ILmr 168/147;
initial I1mr 102,105/80,092 and ILmr 1,135/902; references I1mr 287/276 and
ILmr 156/123. Initial cache increases are +27.49% I1mr and +25.83% ILmr.
Raw evidence remains at `/tmp/bend2-architecture-callgrind-optimized-12`.
No performance retention approval or hosted x86_64 parity is inferred.

The corrected release completed seven alternating local macOS ARM latency rounds,
32 samples and eight warmups per workload. This report excludes real compiler
execution and initialization; compiler-unavailable diagnostics establish exact
revision readiness. Median-of-round p50/p95 milliseconds, baseline/candidate:

| Workload | p50 | p95 |
| --- | --- | --- |
| Warm hover | 0.064/0.106 | 0.248/0.487 |
| Warm definition | 0.056/0.098 | 0.375/0.234 |
| Warm completion | 0.064/0.096 | 0.256/0.220 |
| Large open to hover | 16.454/16.237 | 24.865/33.952 |
| Large edit to hover | 17.963/17.174 | 29.850/33.447 |
| Hover during large edit | 1.884/0.661 | 1.910/0.846 |
| Hover during large open | 1.350/0.798 | 1.362/0.884 |

Warm median overhead is 0.032–0.042 ms; busy-hover median latency improves,
while large open/edit p95 remains worse. Numerical latency is report-only, not a
substitute for the failed cache gates. Samples and reports remain under
`/tmp/bend2-architecture-latency-optimized-12-{baseline,candidate,report}.json`
and `/tmp/bend2-architecture-latency-optimized-12-report.md`.

### Lookup isolation and rejected candidate-column experiment

PR #16 source `5b764a3` separates borrowed external reference group lookup from
materialization through the same production traversal. The new cases use valid
current symbol identities and zero, one, or three matching sources at 100,
1,000, and 10,000 files. The absent case interns the member name through an
independent file's relation so it reaches an absent bucket. Setup checks exact
sources and occurrence counts outside measurement; the query clones no
`Document` or URI and constructs no occurrence/protocol output.

[Hosted run 37339668833](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37339668833)
measured lookup `Ir` as 462/462/462 for absent, 549/545/549 for one source,
and 659/655/659 for three. Raw lookup profiles contain no executed allocation
or document-materialization records. These workloads do not support a
workspace-size-driven bucket bottleneck. Existing warm reference totals include
materialization and sorting; they are not isolated lookup measurements.
The unchanged main comparator passed 23/36 workloads; the run remained red.
Hosted quality and latency validity passed.

Exclusive nine-event attribution of retained `69316b3` profiles matched profile
totals. Initial build added 23,733 `D1mr` in `prepare_semantic_snapshot` and
8,591 in `occurrence_template_target` versus its same-run main baseline.
This motivated one cold candidate-column experiment, not a warm CSR or arena.
Source `cf89172` recorded unresolved qualified reference ordinals during syntax
construction, avoiding full reference-row iteration and repeated qualifier
resolution during staging. Empty candidate payloads allocated nothing.

[Hosted run 37341424990](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37341424990)
reduced initial-build `D1mr` 157,992→150,646 (−4.65%), but increased incremental
fixture `Ir` 447,514→489,865 (+9.46%) and `D1mr` 5,885→7,455 (+26.68%).
Exclusive added `Ir` included `malloc_consolidate` 28,457 and `unlink_chunk`
9,315. This benchmark includes whole-fixture teardown: those costs are not an
isolated contribution-update measurement. The main comparator passed 21/36
workloads and stayed red. The experiment was rejected and reverted in `3c7e221`;
borrowed lookup and its benchmarks remain. Raw evidence for both sources is
preserved; no baseline, threshold, old workload, or retry-to-green was changed.

### Reference output stage isolation

Reference output benchmarks use the production ordinal resolver, unsorted
materializer, comparator, and deduplication predicate. Borrowed groups retain
their immutable snapshot so ordinals cannot be paired with another database.
This adds one borrowed pointer to the group payload, not a cold owning column;
lookup counts must be remeasured rather than carried forward unchanged.

The compact-range case starts with prepared borrowed groups and collects
`(FileId, TextRange)` without cloning documents or URIs. The materialization
case includes lookup and the existing owned `WorkspaceOccurrence` output but
excludes sorting and deduplication. Sort-only inputs are prepared in canonical,
reverse, and fixed Fisher-Yates order. Dedup-only cases include unique rows and
duplicate runs at the first, middle, and last canonical keys. Setup checks exact
URI/range/kind membership, current target identity, and full-query equivalence.
Existing workloads and regression limits are unchanged.

Private source-included server helpers measure actual `Location` construction:
the binding path clones URIs, the owned path moves them, and the full symbol
pipeline additionally performs its existing protocol sort/dedup. ASCII and
Unicode/CRLF setup cases check exact UTF-16 positions. Protocol counts are
separate: `workspace_references_warm` does not include LSP conversion.

Setup, input clones/permutations, and assertions occur outside the exact
benchmark-helper entry point; returned vectors are destroyed after it returns.
Temporary documents in materialization, removed rows in dedup, and consumed
input document fields in conversion are destroyed inside measurement.
Independent stage totals are not an additive partition: prepared heap state,
traversal order, and code generation differ between workloads.

The initial instrumentation source `991a7f4` passed semantic setup and stdio
checks, but its ARM raw stage profiles exposed a collection-boundary failure:
sort-only profiles included retained-output frees, and compact mapping omitted
part of the row traversal. Its new-stage totals are not valid decomposition
evidence. Iai's default toggle is `*::__iai_callgrind_wrapper_mod::*`; generated
iterator/drop code can also match that wildcard. New cases therefore select
exact, non-inlined benchmark-helper entry points instead. Existing cases keep
their original entry points, data, measurement scope, and active policy.

The corrected persistent Linux ARM smoke executes all 121 cases and their
semantic assertions. Raw profiles for all nine sort cases and three unique
dedup cases contain no executed frees, output-drop routines, or Arc decrements.
Compact mapping also uses explicit loops rather than callbacks defined in the
entry-point function: exact-entry raw runs visit all six sparse or 198 matched
rows. A disposable native helper smoke independently checks exact compact
values, Call/Read kinds, ordering/deduplication, and Unicode/CRLF protocol ranges
for three and 99 sources. ARM counts are smoke evidence, not hosted acceptance.

Retained source `5b764a3` sparse-10,000 raw profiles attribute 5,523 of 9,307 `Ir`
to exclusive allocator-function records (59.3%), with nine-event sums checked
against the profile total. This motivates materialization attribution, not a
bucket redesign. Actual stdio before/after extraction preserves exact reference
and rename ranges through Unicode/CRLF, interleaved aliases and qualifier
shadowing, local/self-import uniqueness, dependency overlay removal, and disk
restoration on close; both executions exit cleanly.

### Bounded reference materialization experiment

The stage evidence motivates eliminating transient `Document` construction,
not changing global identity, bucket organization, snapshot ownership, or
ordering. The former materializer eagerly creates a target document even when
there are no local result rows, and caches an owned source document before
cloning it again for each external occurrence. The candidate instead borrows
target metadata and caches borrowed source entry/snapshot metadata. It creates
owning URI/language/snapshot handles only for returned occurrences.

The returned representation, vector growth strategy, canonical URI/byte-range
comparator, dedup predicate, and protocol conversion remain unchanged. The
immutable workspace borrow keeps each ordinal and returned document on the
same current snapshot. Exact native helper values and actual stdio reference,
rename, Unicode/CRLF, shadow/alias, local/self-import, and dependency-overlay
transitions pass after the change. The bounded change is retained for its
demonstrated warm-query result below, not as an overall acceptance claim.

Corrected stage baseline `bcaefa8`
([run 37351693962](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37351693962))
measures sparse-10,000 lookup at 671 `Ir`, prepared-group compact mapping at
872, owned materialization at 6,392, sort at 269/1,479/1,083 for
canonical/reverse/fixed-shuffle inputs, and unique dedup at 197. Binding
`Location` conversion costs 3,810, pure URI-moving conversion 2,633, and the
additional symbol protocol pipeline 3,101. These are independent stage
workloads, not an additive explanation of the full 9,789-instruction query;
protocol serialization/transport is not measured. The baseline main comparator
passes 29/36 workloads and remains red.

Candidate `b642de8`
([run 37352647124](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37352647124))
reduces unsorted materialization `Ir` 6,653→5,009 at sparse-1,000 (−24.71%),
6,392→4,824 at sparse-10,000 (−24.53%), and 232,893→193,688 at matched-100
(−16.83%). All six unchanged full warm-reference cases improve:

| Full warm references | Baseline `Ir` | Candidate `Ir` | Change |
| --- | ---: | ---: | ---: |
| sparse-100 | 9,416 | 7,651 | −18.74% |
| sparse-1,000 | 9,581 | 7,816 | −18.42% |
| sparse-10,000 | 9,789 | 7,604 | −22.32% |
| matched-100 | 338,659 | 284,009 | −16.14% |
| matched-1,000 | 3,717,873 | 3,402,477 | −8.48% |
| matched-10,000 | 44,242,253 | 40,432,680 | −8.61% |

Exclusive sparse-10,000 materialization savings include `FileEntry::document`
384 `Ir`, `malloc` 348, `_int_free` 432, and `free` 248. Full-query sorting costs
also differ despite an unchanged comparator: traversal order and allocation
state are not fixed between these query runs. Do not attribute the entire
full-query delta to document construction. Controlled shuffled sorting and
unique dedup remain instruction-identical; all three sparse-10,000 protocol
stage counts are unchanged.

Initial/incremental/burst fixture `Ir` changes −0.0099%/+0.0107%/−0.0019% versus
the corrected baseline, with effectively unchanged data-cache read misses.
Cold snapshot `Ir` stays effectively unchanged, but its `I1mr` grows
3.22–3.48%; matched-100 semantic-build `I1mr` grows 3.95%. These instruction-cache
tradeoffs remain visible, not waived or described as data-locality improvements.
The candidate main comparator passes 25/36 workloads and remains red. Both
sources pass hosted quality and latency validity. Raw profiles for initial,
corrected, and candidate sources are preserved; no identical-source performance
retry, threshold/baseline change, merge, or release is inferred.

## Primary sources

[original-index]: https://index.crates.io/to/we/tower-lsp
[fork-index]: https://index.crates.io/to/we/tower-lsp-server
[original-manifest]: https://github.com/ebkalderon/tower-lsp/blob/v0.20.0/Cargo.toml
[original-transport]: https://github.com/ebkalderon/tower-lsp/blob/v0.20.0/src/transport.rs
[original-pending]: https://github.com/ebkalderon/tower-lsp/blob/v0.20.0/src/service/pending.rs
[original-layers]: https://github.com/ebkalderon/tower-lsp/blob/v0.20.0/src/service/layers.rs
[fork-manifest]: https://github.com/tower-lsp-community/tower-lsp-server/blob/v0.23.0/Cargo.toml
[fork-lib]: https://github.com/tower-lsp-community/tower-lsp-server/blob/v0.23.0/src/lib.rs
[fork-transport]: https://github.com/tower-lsp-community/tower-lsp-server/blob/v0.23.0/src/transport.rs
[fork-pending]: https://github.com/tower-lsp-community/tower-lsp-server/blob/v0.23.0/src/service/pending.rs
[fork-layers]: https://github.com/tower-lsp-community/tower-lsp-server/blob/v0.23.0/src/service/layers.rs
[fork-changelog]: https://github.com/tower-lsp-community/tower-lsp-server/blob/v0.23.0/CHANGELOG.md
[fork-uri]: https://docs.rs/ls-types/0.0.6/ls_types/struct.Uri.html
[rc-manifest]: https://github.com/tower-lsp-community/tower-lsp-server/blob/v0.24.0-rc.1/Cargo.toml
[rc-transport]: https://github.com/tower-lsp-community/tower-lsp-server/blob/v0.24.0-rc.1/src/transport.rs
[rc-changelog]: https://github.com/tower-lsp-community/tower-lsp-server/blob/v0.24.0-rc.1/CHANGELOG.md
[tower04-lib]: https://github.com/tower-rs/tower/blob/tower-0.4.13/tower/src/lib.rs
[tower05-lib]: https://github.com/tower-rs/tower/blob/tower-0.5.2/tower/src/lib.rs
[tower04-manifest]: https://github.com/tower-rs/tower/blob/tower-0.4.13/tower/Cargo.toml
[tower05-manifest]: https://github.com/tower-rs/tower/blob/tower-0.5.2/tower/Cargo.toml
[lsp-order]: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#messageOrdering
[lsp-cancel]: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#cancelRequest
