## Summary

On the Intel macOS (`x86_64-apple-darwin`) diagnostic evidence job of PR #62 at source head `420a77c5da3ae808204b506d6d78bb955cd1113e`, the native-heap diagnostic (xctrace Allocations) failed before the LSP was ever launched. The preflight Time Profiler capture, attached to the harness process rather than the LSP, printed that it reached its 1 s time limit and then did not exit: the owned xctrace child was killed and reaped at the 300 s deadline.

The owner has classified this recorded profiler failure as **non-blocking for this run only**. That classification is limited to this case: it is not a performance-regression acceptance, not an approval or merge decision, and it does not change the failed workflow result. No retry, deadline, bound, fixture, dataset or resource change is part of this issue.

## Source identity (frozen)

- Source head: `420a77c5da3ae808204b506d6d78bb955cd1113e`
- PR #62 verified merge: `ccca016a83ea61148c231d6caa1ea0bf8e21e656`
- Verified merge tree: `642cbe20a98089bb991b9b77f6134af60ae43e3d` (identical to the source head tree)
- Source unchanged during observation; 0 reruns; no deadline or resource changes.
- Workflow run: https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/41173453571 (`perf-diagnostics`, conclusion: failure)
- Failing job: `124456402828` — `diagnostic evidence (x86_64-apple-darwin)` — https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/41173453571/job/124456402828
- Artifact: `13034850045` `perf-pr-41173453571-1-x86_64-apple-darwin` (138,104,173 bytes, digest `sha256:7bb1199e29a0e860aac708f746d421ae09279cac459cf1dc69ece421da8d7a5f`) — https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/41173453571/artifacts/13034850045

The other five native diagnostic jobs in this run succeeded. This failure is not covered by the separate historical Windows issue #74; no #74 exemption is claimed.

## Failure stage and deadline

- Scenario: `discovery-10000`, native-heap profile. The scenario never started: `target_pid: null`, `phases: []`, `scenario_started: false`.
- The LSP was never launched for this profile and **no Allocations capture started** (`actual_Allocations_capture_started: false`).
- Actual failing command (recorded in the native-heap profile manifest):

  `xctrace record --template "Time Profiler" --time-limit 1s --no-prompt --output .../tracing-preflight.trace --attach 50236`

- Attached process: `bend2-perf` helper PID `50236`, not the LSP.
- Observed stdout (`command-5.stdout.log`, 183 original bytes):

  `Starting recording with the Time Profiler template. Attaching to: bend2-perf (50236). Time limit: 1.0 s`
  `Ctrl-C to stop the recording`
  `Reached specified time limit, ending recording...`

- Observed stderr: empty (0 bytes).
- Deadline: 300 s. Recorded error: `/Applications/Xcode_16.4.app/Contents/Developer/usr/bin/xctrace timed out after 300s; owned child is killed and reaped`.
- Tool identity: xctrace at `/Applications/Xcode_16.4.app/Contents/Developer/usr/bin/xctrace`, SHA-256 `0b72e2a1da1bd8d05c7c755edfa752a0ad9d7e1a7056aaf1e616cfeb4215d84f`, version `Xcode 16.4 (16F6)`.
- Environment: macOS `15.7.9 (24G830)`, SDK `15.5`, target `x86_64-apple-darwin`.
- Native-heap built binary SHA-256: `b77ac9fd71bc08ec228406c1b8c5384b6bec91b584e09db99ba055a1c0dffede` (unused in this attempt because the LSP never launched).

The instrumented trace directory contains `tracing-preflight.trace/Trace1.run/RunIssues.storedata` (53,248 bytes, SHA-256 `1a2d887ee80799ce4509b5973a0eb6351caabb4909f0e9862fab210da5cb0ac5`). A bounded read-only SQLite fallback (metadata only, `immutable=1`, `query_only`, no extension loading, no WAL sibling in the artifact) reports the CoreData `ZISSUE` table has **0 rows**. That only records the closed database as captured; it is not evidence that finalization succeeded and it does not identify the cause.

## Earlier same-job command template (not an identical-argv control)

An earlier native profile in the same job ran the same Time Profiler 1 s preflight template against a different harness helper PID `46001`, completed in 4.135660781 s, and produced a complete native profile (`profiles/discovery-10000/native/profile-manifest.json`). The attached process, output path and process/output state differ from the failing attempt, so this is **not** a causal identical-argv control and does not establish a regression or a harness defect.

## Source anchors (existing bounded handling)

These anchors describe the current orchestration and bounded process handling only; they do not imply a fix or a proven defect:

- `tools/perf/src/profiling.rs:255` — `session.prepare(&binary)` runs before scenario execution.
- `tools/perf/src/profiling/sessions.rs:354-371` — xctrace preflight, `--time-limit 1s`, attached to the live harness process.
- `tools/perf/src/profiling/sessions.rs:152-163` — bounded `try_wait` loop that returns the `{program} timed out after {n}s; owned child is killed and reaped` error at the deadline.
- `tools/perf/src/profiling/sessions.rs:1552-1560` — `OwnedCommand` drop path kills and waits for the child.

The internal xctrace finalization locus is unavailable, no hung profiler stack was captured, and no Apple bug or repository orchestration defect is established by this evidence.

## Evidence links (immutable)

- Full job log: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel-job.log.gz
- Native-heap profile manifest: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/profiles/discovery-10000/native-heap/profile-manifest.json.gz
- Failing preflight stdout: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/profiles/discovery-10000/native-heap/command-5.stdout.log.gz
- Preflight issue database proof: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/preflight-sqlite-issues.json and https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/preflight-sqlite-schema.json
- Earlier completing native profile manifest (PID 46001 control context): https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/profiles/discovery-10000/native/profile-manifest.json.gz
- Selected original member SHA/CRC proof: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/selected-original-members.json and https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/parent-member-qualification.json
- Full immutable evidence manifest: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/manifest.json

Artifact coverage: 21 selected complete original members were verified through HTTP Range (original member CRC32 and uncompressed size, plus artifact manifest SHA-256 and size). This is **not** a whole-archive or whole-profile-corpus revalidation claim.

## Missing facts / open questions

- Why xctrace did not exit after reporting that it reached the 1 s time limit is unknown.
- No stack or sample of the hung xctrace process was captured; the preflight trace issue database is empty.
- Whether the stall is specific to attaching to the harness process under this hosted Intel macOS environment, or to something else, is unresolved.
- No Allocations capture, LSP launch or scenario data exists for this attempt.

## Acceptance for future investigation

A future investigation should determine, from reproducible evidence, why this 1 s preflight Time Profiler capture failed to finalize for 300 s after reaching its time limit while attached to harness PID `50236`, and whether a bounded, evidence-backed remediation exists. Any deadline, bound, fixture, dataset or orchestration change would require a separate explicit decision and is not authorized by this issue. Until then the original workflow remains failed and this issue is non-blocking by the current owner decision for this recorded case only.
