# Opt-in Chrome/Perfetto timeline tracing

`bend2-lsp` can emit a Chrome trace-event JSON file for timeline analysis. Tracing is disabled by default. It does not create a default trace file or activate the separate compiler-metrics writer.

## Build and enable

Build the LSP server:

```sh
cargo build --release --locked
```

Set `BEND2_LSP_TRACE` in the environment of the `bend2-lsp` process to the full output filename. The file is created or truncated at startup:

```sh
BEND2_LSP_TRACE="$PWD/target/bend2-trace.json" ./target/release/bend2-lsp
```

Unset or empty `BEND2_LSP_TRACE` means no trace writer and no trace file. If the path cannot be opened or tracing cannot initialize, the server continues without tracing and writes only the operating-system error kind to stderr.

### Editor launch

Set the variable on the editor/LSP-client process that launches `bend2-lsp`; setting it in an unrelated terminal does not affect an already-running editor. For VS Code with the `code` command installed, close existing VS Code instances and launch a fresh process from a shell:

```sh
BEND2_LSP_TRACE="$PWD/target/bend2-trace.json" code --new-window .
```

Alternatively, use the editor extension's LSP server launch configuration to add this environment variable to the `bend2-lsp` child process. This repository does not define an editor extension or a `settings.json` key for server environment variables.

## Capture a representative session

1. Start the editor with `BEND2_LSP_TRACE` set and open the project in which you want to investigate latency.
2. Open a substantial source file with several local imports. Wait for initial diagnostics to finish.
3. Exercise the requests of interest: hover, completion, references, call hierarchy, and a workspace-symbol query.
4. Edit the active file, then edit a shared imported file and wait for dependent diagnostics to publish.
5. Shut down the LSP client normally. Allow it to send LSP `shutdown` followed by `exit`, then wait for the `bend2-lsp` process to exit. The server also finalizes the trace when its stdin closes or it receives SIGTERM, including SIGTERM after a `shutdown` response. SIGKILL cannot run cleanup and may leave the JSON array incomplete. Dropping the tracing flush guard writes the closing delimiter and joins the writer thread.

On Unix, SIGTERM is a graceful shutdown request: the server cancels outstanding analysis/compiler work, flushes the trace, and exits with status 0 even without an LSP `shutdown` message. SIGKILL bypasses cleanup. Zed-specific termination mechanism: unresolved.

Open the completed `bend2-trace.json` in [Perfetto UI](https://ui.perfetto.dev/) using **Open trace file**. The output is Chrome trace-event JSON. Async spans are represented by matching `b`/`e` events and may resume on another Tokio worker thread; view them as spans rather than assuming one thread owns the whole operation.

## Span guide

- `lsp.request` and `lsp.notification`: static LSP method names. Document requests may also record a numeric, process-local `FileId` and document revision.
- `transport.service_call` and `transport.service_future`: decoded request entering the tower-lsp service and its future resolving or being dropped. Both carry the same process-local correlation number, a static method category, and a JSON-RPC ID only when it is numeric. The future span includes async waiting; it is not CPU time.
- `transport.stdin_read` and `transport.stdin_chunk` events, plus `transport.stdout_write` and `transport.stdout_flush` spans: raw I/O chunk boundaries and writes. A chunk may split or combine frames, and writes have no request ID; do not assign one to a particular request when messages overlap. Tower-lsp owns framing, decoding, response serialization, and its response queue: the interval from a completed frame at the client-side proxy to `transport.service_call`, and the interval from `transport.service_future` to the first output write, still include unmeasured library work.
- `document.update`: `didOpen`, `didChange`, and `didClose` lifetimes.
- `document.wait_revision`: time waiting for the requested document revision to become committed; records desired/committed revisions and an outcome.
- `workspace.readiness_wait`: `references` and `rename` wait for pending document revisions captured when the request starts, before reading the committed workspace. It records only the number of captured pending revisions; later updates are not included.
- `workspace.sync_wait`: time waiting for the shared workspace read guard.
- `navigation.*`: go-to-definition document/prelude, cursor/import/token, module/declaration, and location-construction phases. `navigation.declaration_lookup` and `navigation.location` use only static `local`, `prelude`, or `imported`/`import` scope labels; no token or location data is recorded.
- `snapshot.build`: immutable snapshot construction; records revision, source byte count, and existing token/import/symbol/call counts.
- `workspace.update`, `workspace.commit`, and `workspace.query`: staging/synchronization, synchronous DB commit sections, and workspace queries. Commit/query spans include only scalar counts and process-local file IDs.
- `analysis.query`: selected indexed analysis work, including document-symbol and reference queries; records result counts.
- `compiler.check`, `compiler.stage`, and `compiler.child`: source-graph compiler checks, staging file/byte counts, cache state, child outcome, and safe exit status. Child output and command arguments are not captured.
- `diagnostics.run` and `diagnostics.publish`: the asynchronous diagnostics task and document/imported/clear publication counts and outcomes. Diagnostics messages are not included.

## Privacy and scope

Trace fields are explicitly allowlisted. They contain static span/method names, revisions, numeric counts and numeric JSON-RPC IDs, process-local correlation numbers and `FileId`s, booleans, outcomes, and a child exit code. They do not contain source text, identifiers, labels, diagnostic messages, filesystem paths, URIs, compiler paths/arguments, stdout, or stderr. The Chrome writer adds generic process/thread metadata (for example `main` or `tokio-rt-worker`); no source locations are enabled. Review a trace before sharing it.

`BEND2_LSP_COMPILER_METRICS_FILE` remains independent. Setting it still writes the existing compiler metrics rows whether or not tracing is enabled; tracing alone does not activate that writer.

## Release size and measured overhead

The maintainer accepts the release-size increase to keep tracing built into the production executable: `BEND2_LSP_TRACE` enables recording on the exact binary used by the editor. The tracing dependencies remain unconditional; no feature split or separate tracing build is required.

| Release executable | Size | Delta from pre-tracing |
| --- | ---: | ---: |
| Pre-tracing | 2,937,584 bytes | — |
| Current | 3,319,744 bytes | +382,160 bytes (+13.01%) |

The pre-tracing artifact and current executable use the same Rust 1.98.1 release profile. The current executable had SHA-256 `0ee7df3700989c7202c475b7fda7591fd137b5122c7ce140f5fb13461fa35afb` in both tracing-off and tracing-on runs; the pre-tracing artifact hash was `702a4c0c50a53ba7f24fece385de0986fc65cd8c4d2db1187d050782dcf97ff0`.

Protocol measurements use a local stdio LSP client, a fixed document with 40 callers, and a compiler stub that exits successfully. Startup includes process creation through the initialize response; shutdown includes the shutdown request through process exit. Warm-request rows pool 150 calls per operation over 10 process sessions. Large-edit rows pool 30 full replacements of a 1,041,743-byte document, each followed immediately by hover on that document. Percentiles use nearest-rank p95. These are process-protocol timings, not editor UI timings.

| Scenario (p50 / p95 ms) | Pre-tracing | Current, env unset | Current, tracing enabled |
| --- | ---: | ---: | ---: |
| Startup to initialize (30 runs) | 10.9238 / 19.7810 | 10.5025 / 17.5776 | 9.7003 / 14.9020 |
| Shutdown request to exit (30 runs) | 1.7156 / 11.0123 | 1.6786 / 13.9301 | 1.5633 / 11.7467 |
| Warm hover (150 calls) | 0.0990 / 1.4632 | 0.0920 / 0.7920 | 0.1021 / 0.5059 |
| Warm completion (150 calls) | 0.0882 / 1.3708 | 0.0798 / 0.6129 | 0.0926 / 0.4460 |
| Warm semantic tokens (150 calls) | 0.1699 / 1.0657 | 0.1471 / 0.6679 | 0.1607 / 0.8955 |
| Warm references (150 calls) | 0.2155 / 1.0216 | 0.1792 / 0.8235 | 0.1976 / 1.3403 |
| 1 MB `didChange` then hover (30 runs) | 26.0726 / 125.8978 | 23.2607 / 42.9858 | 25.0175 / 53.9299 |

With tracing disabled, measured medians were at or below the pre-tracing medians for every scenario. The disabled shutdown p95 was 2.9 ms higher; the process-start and large-edit samples also contain scheduling outliers. No persistent disabled-path regression is visible in these local samples. Enabled recording is not a performance gate.

A short enabled session (initialize, open one document, one hover, completion, semantic-token, and reference request, graceful shutdown) produced 10,670 bytes and 70 trace events: 33 async-span begin events, 33 matching end events, and 4 metadata events across 15 span names. The repeated workload process produced a median 87,671-byte trace with 573 events (15 measured calls of each warm request and 3 large edits).

Tracing initialization reads `BEND2_LSP_TRACE` once at server startup. With it unset or empty, initialization returns before opening a file or constructing the Chrome layer/writer; the no-subscriber tracing callsites remain no-ops.

Tracing is for locating work and waiting boundaries, not for sampling CPU stacks. Once a timeline identifies a hot interval, use a separate sampling profiler against the server process and repeat the same editor scenario:

- **macOS:** Attach Instruments **Time Profiler** to the editor-launched `bend2-lsp` PID. Leave its default 1 ms sampling interval initially; samples shorter than the interval may be missed. The command-line equivalent is:

  ```sh
  xcrun xctrace record --template "Time Profiler" --attach "$PID" --output bend2-time-profiler.trace --time-limit 60s
  ```

  See [Apple's xctrace guide](https://developer.apple.com/videos/play/wwdc2022/10106/).
- **Linux:** Attach `perf` to the editor-launched server PID. `-F 99` requests 99 samples/second; DWARF call graphs need kernel permissions and suitable `libunwind`/`libdw` support:

  ```sh
  sudo perf record -F 99 --call-graph dwarf -p "$PID"
  # Exercise the same editor scenario, then press Ctrl-C.
  sudo perf report
  ```

Sampling capture is intentionally manual and is not automated by this feature.
