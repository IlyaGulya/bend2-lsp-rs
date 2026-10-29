# Windows x64 saved-lint timeout: complete retained evidence

**Separate non-blocking follow-up for PR #62, explicitly requested by the maintainer.** The original failed run remains failed. No test, deadline, concurrency, retry policy, CI check or protection is changed. No causal fix is claimed.

## Original observation

- System under test: `c7d0b2c2a9ee88a329433ef01ce496c3892f3985`; tested PR merge `941c8988b6799a784b587dbcb8c923a0164ace28` has the identical Git tree.
- [Original full quality run40863668428](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/40863668428), [Windows2022/x64 job123412904770](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/40863668428/job/123412904770).
- Command `cargo test --locked --test bend_lint_protocol`, default concurrency: 28 passed,1 failed. Case `timeout_kills_analyzer_and_its_descendant_without_blocking_compiler` panicked `fixture did not create address` at original test line234.
- [Original wait_file](https://github.com/IlyaGulya/bend2-lsp-rs/blob/c7d0b2c2a9ee88a329433ef01ce496c3892f3985/tests/bend_lint_protocol.rs#L228-L236), [unchanged500ms case/assertions](https://github.com/IlyaGulya/bend2-lsp-rs/blob/c7d0b2c2a9ee88a329433ef01ce496c3892f3985/tests/bend_lint_protocol.rs#L603-L621).
- Intended behavior: analyzer and descendant publish socket addresses; timeout kills/reaps both; compiler version2 remains responsive; no stale lint publication; clean shutdown.
- Original uninstrumented failure did not retain timed LSP messages, server stderr, fixture startup/control timeline or process-start/exit trace. The precise cause cannot be recovered from those receipts alone.

## Complete primary files

`manifest.json` binds every retained file by byte size and SHA256. `original-pr-quality-full-windows-job.log` is the complete primary job log downloaded through `gh run view --job --log`; the original failed-step log and complete run metadata are also retained. ZIPs are original authenticated artifacts, not reassembled/sanitized projections. All original members and every ZIP CRC were checked. Job/artifact URLs are convenient originals; these committed copies do not depend on GitHub Actions artifact retention.

## Diagnostic history (failures remain failures)

1. Producer `1e71e2c332fa5a69aea55d7af754a999d91bcca5`, [one-shot40884115926](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/40884115926), artifact12904505751. Observer reached readiness then disappeared between workflow steps. **SUT tests were not run.** Observer stderr and end-summary absent; no external killer is attributed. Aborted-run rendering separately indexed a missing array. Its [full quality40884116542](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/40884116542) failed on diagnostic logging's `clippy::map_unwrap_or`; all7 native/legacy jobs passed. Original logs, metadata and ZIP are preserved here.
2. Producer `d939306d639a4587c975bea6146381c75bc90691` fixes diagnostic ownership (start/test/identity-bound stop in one workflow shell), aborted-summary null guard and the logging Clippy expression; no production or fixture changes. [Commit](https://github.com/IlyaGulya/bend2-lsp-rs/commit/d939306d639a4587c975bea6146381c75bc90691), [collector/workflow](https://github.com/IlyaGulya/bend2-lsp-rs/blob/d939306d639a4587c975bea6146381c75bc90691/.github/workflows/windows-lint-startup-diagnostic.yml), [instrumented test](https://github.com/IlyaGulya/bend2-lsp-rs/blob/d939306d639a4587c975bea6146381c75bc90691/tests/bend_lint_protocol.rs).
3. [Actual observed40892502856](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/40892502856), artifact12905157215, ZIP SHA256 `5316eca6250a3dd44f49497d61c488170896066f8efe66c169fa9744a4a3a3c9`: all29passed,0ignored/filtered/retries,5.42s. All291 covered tracked source files match C7/producer/before/after; instrumented test SHA256 `b000a6a0b96db85fa2c5a016a6d0e75b12d1f36761caf3145f7a0f2fcbb63a5a`; identity gate passes. Both CIM subscriptions active,1332 callback records, no dropped/overflow/malformed/watcher errors, complete stop-requested end-summary, observer gone. Original failure **not reproduced**.
4. [Required full quality40892502216](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/40892502216): all8jobs pass, actual `./scripts/quality`. This qualifies the diagnostic producer, not a causal repair of the original failure.

## Controlled mechanism probe — not Windows attribution

The included authored Rust startup wrapper and Python real-LSP probe ran against unchanged C7 production in the existing persistent Linux container. With0ms injected prepublication delay both analyzer/descendant sockets appear and500ms timeout kills/reaps both. With750ms injected before original fixture publication the real500ms timeout kills/reaps the analyzer before any calls/address files; compiler version2 remains responsive, no lint finding, clean exit. Exact command/output/binary/fixture hashes are in `linux-mechanism-probe-run.json`. This establishes one POSSIBLE mechanism only, not the original Windows cause, Windows performance, or a source fix. All temporary container probe binaries/scripts were removed after the run.

## Follow-up acceptance

- Capture a failing Windows x64 startup with timed LSP messages plus control-file/process-start/exit evidence; distinguish actual500ms cancellation from spawn/job/capture/I/O/panic/early-exit causes. Do not attribute host load or process startup without evidence.
- Fix the proven cause and keep500ms, all29cases, default concurrency and every root/descendant cleanup, compiler responsiveness, stale-publication and shutdown assertion.
- Keep deterministic failing-before/passing-after regression where practical; no retries-to-green, test disabling/serialization, deadline increase, check bypass or source-specific fake fallback.
- Preserve the original red run and failed diagnostic attempts. A later observed pass is not a causal explanation.

The older already-fixed empty-address atomic-publication race had a DIFFERENT signature; do not conflate it with this missing-file failure. These files are data and source reproducers only; nothing in this branch is installed or executed automatically.
