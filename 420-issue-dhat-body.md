## Summary

On the Intel macOS (`x86_64-apple-darwin`) diagnostic evidence job of PR #62 at source head `420a77c5da3ae808204b506d6d78bb955cd1113e`, the DHAT heap profile of the `discovery-10000` scenario failed. Initialize and local completion completed; the first cold `workspace/symbol` discovery request then exceeded the existing 30 s RPC deadline and the run was recorded as failed:

`LSP channel wait failed: timed out waiting on channel`

This is a current cold-start case. It is **not** a claim that the warm-reference allocation bug tracked by #52 has returned: #52 was closed as completed after the warm per-comparison URI-key allocation work, while this failure occurs in the first cold workspace/symbol phase before any warm iteration. This issue links #52 only as prior related context.

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

- Backend: DHAT 0.3.3 (embedded allocator, profile format v2); scenario `discovery-10000`; scenario target PID `45617`.
- Profiled DHAT binary SHA-256: `7208fdad5cd5fd61674ea49736ecb910639b1bc4f65ae3bf3da2a338ecb1f058` (the CPU and native Time Profiler control processes use `feb19e02e8339a93fa744a3e28771490defddb555e48b78c71f36374546874d5`).
- `initialize` and `local_completion` phases completed. Manifest phase markers: `initialize.before` 7.137064330 s → `initialize.after` 7.158311395 s; `local_completion.before` 7.158322532 s → `local_completion.after` 7.188339088 s.
- `cold_discovery.before` at 7.188343471 s; `request_timeout.before_abort` at 37.273730216 s, i.e. about 30.085 s in the cold discovery phase, against the 30 s RPC deadline.
- Result: `status: failed`, `heap_summary: null`, no DHAT heap output because the process was terminated after the timeout.
- A live `/usr/bin/sample` capture (1 ms interval, 1 s) wrote `request-timeout.sample.txt`: 20,902 original bytes, 7 threads with two observations per thread (sparse sample; main thread and other workers parked).

Observed stack in that sparse sample (data, not a conclusion):

- Discovery worker in `WorkspaceDb::admit_discovered_path` (`workspace.rs:427`) calling `Url::from_file_path` with the allocation inside the DHAT `GlobalAlloc::alloc` → `new_backtrace_inner` backtrace path.
- Discovery worker in `WorkspaceDb::sync_disk_snapshot_prepared` (`workspace.rs:1356`) inside DHAT bookkeeping (`get_pp_info`/`new_heap`) and allocator free path.

Limits: a sparse sample with allocation instrumentation active is not proof of a deadlock, not default-binary latency, not evidence of overall CPU dominance, and not proof of a source regression. The observed stacks are diagnostic observations only.

## Same-host controls and their limits

All three observations are from the same job, same source/merge/tree, same `discovery-10000` scenario, with a dataset manifest exactly equal for all 10,000 file names, sizes and SHA-256 values; 32 samples and 8 warmup iterations where the scenario completed.

| Profile | Binary SHA-256 | Cold discovery observed | Outcome |
| --- | --- | --- | --- |
| samply CPU profile | `feb19e...4d5` | 2.81338278 s | complete |
| native Time Profiler capture | `feb19e...4d5` | 5.751843036 s | complete |
| DHAT | `7208fd...058` | 30.08536584 s recorded (manifest marker delta 30.085386745 s) | failed, 30 s RPC deadline |

The timings are **whole `cold_discovery` phase transitions**, not isolated RPC times and not clean latency. On successful runs that transition also includes cold reference work and validation, so these numbers are not directly comparable as per-request latency. The three processes were independent, differently instrumented, and run at different times: this is not a randomized control, and the observed breach is specific to the DHAT capture in this job. It does not prove a default-build regression or uniquely isolate an internal cause.

## Evidence links (immutable)

- Full job log: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel-job.log.gz
- DHAT profile manifest: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/profiles/discovery-10000/heap/profile-manifest.json.gz
- Live timeout sample: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/profiles/discovery-10000/heap/request-timeout.sample.txt.gz
- Scenario definition: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/profiles/discovery-10000/heap/scenario/scenario.json.gz
- Same-host profile controls: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/same-host-profile-controls.json
- Selected original member SHA/CRC proof: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/selected-original-members.json and https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/hosted/diagnostics/macos-intel/parent-member-qualification.json
- Full immutable evidence manifest: https://github.com/IlyaGulya/bend2-lsp-rs/blob/bd3532658a42a4d9822c7955a9b16b2810cdc8f1/manifest.json

Artifact coverage: 21 selected complete original members were verified through HTTP Range (original member CRC32 and uncompressed size, plus artifact manifest SHA-256 and size). This is **not** a whole-archive or whole-profile-corpus revalidation claim.

## Missing facts / open questions

- No DHAT heap summary or DHAT output exists for this attempt because the target was terminated at the RPC deadline.
- No clean latency measurement, randomized control or default-binary comparison is available from this run.
- The sparse sample cannot distinguish slow allocation-instrumented progress from a blocked path.
- Whether the DHAT-instrumented first cold discovery of 10,000 files can complete inside the existing 30 s gate on Intel macOS is unresolved.

## Acceptance for future investigation

A future investigation should determine, from reproducible evidence, why the first cold workspace/symbol discovery on the Intel macOS DHAT build exceeded the existing 30 s deadline while the same-host CPU and native captures completed, and whether there is an actionable cause. Any deadline, bound, fixture, dataset or instrumentation change would require a separate explicit decision and is not authorized by this issue. Until then the original workflow remains failed and this issue is non-blocking by the current owner decision for this recorded case only.

Related prior context: #52 (closed; warm per-comparison URI-key allocations fixed in PR #56). That warm-phase fix is not claimed here and this issue does not reopen or modify #52.
