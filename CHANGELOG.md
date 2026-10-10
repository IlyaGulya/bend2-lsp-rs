# Changelog

## Unreleased

### Features

- Discover unopened regular `.bend` files under workspace roots in the background
  using existing immutable snapshots and workspace indexes. Include them in
  workspace symbols, references/rename, auto-import completion, and quick fixes.
- Respect ignore rules and discovery exclusions without restricting explicit
  imports. Preserve unsaved overlays across watched create/change/delete events,
  workspace-folder removal, and merged file-renaming identities.
- Own, supervise, cancel, and drain discovery workers; cancelled feature requests
  cannot abandon pending indexing, and stale scans cannot restore removed roots.

### Bug Fixes

- Resolve references from discovered closed declarations and importers without
  requiring the queried file to be opened in the editor first.
- Return local completion while initial workspace discovery is still running,
  marking partial lists incomplete so editors can refresh project candidates.
  Preserve completion readiness after preceding watched-file updates.
- Ignore unrelated and no-longer-reachable watcher events before file reads or
  indexing. Release orphan user-owned workspace snapshots when imports are
  removed or documents close, including cycles.
- Reject stale staged disk loads across close and re-import transitions without
  dropping independent valid updates. Reload current disk contents when a
  still-imported unsaved user-owned buffer closes.
- Keep exact compiler-result cache matching without retaining complete semantic
  snapshots after checks finish.
- Preserve closed consumer references when a filesystem move merges an already
  loaded destination snapshot into an existing file identity.

### CI

- Run all new performance measurements on hosted CI only. Collect alternating
  baseline/candidate LSP latency and workspace-discovery evidence on all six native
  platforms, plus separate symbolized DHAT server/example allocation profiles.
  Preserve raw samples, source/binary provenance, and existing Callgrind gates.
- Observe completion during initial discovery and cold/retained/released memory
  in 10, 1000, and 10000-file workspaces. Preserve timestamped resident-memory
  samples on Linux/macOS/Windows with distinct native footprint/private-commit
  definitions; distinguish sampled peaks, lifetime high-water marks and DHAT
  allocations. Missing required observations make reports incomplete.
- Buffer and atomically replace performance JSON checkpoints so interrupted
  collection preserves previously completed results instead of truncating them.
- Replace Python performance collectors, comparators, reports, and calibration
  with an unpublished Rust tooling crate and a shared portable JSON-RPC transport.
  Preserve contract coverage and regression thresholds; keep tooling separate
  from production server binaries and measured analysis functions.
- Add unified `cargo perf doctor`, hosted `compare`/`profile`, and validated
  `open` commands with offline HTML/JSON/Markdown reports and exact-run downloads.
- Add separate samply CPU, Linux perf, macOS Instruments, and Windows WPR/WPA
  diagnostic captures with target-process evidence, packaged symbols and
  cooperative cancellation cleanup. Preserve original native trace formats.
- Keep ordinary PR measurements lightweight; retain all thirteen DHAT workloads
  and four shared scenarios in full diagnostic/weekly coverage, with six-target
  backend verification for performance-infrastructure changes.
- Preserve report command-log hashes during rendering and keep CI job summaries
  within GitHub's upload limit; full reports and raw evidence remain in artifacts.
- Deliver sampler stop signals to the owned recorder on macOS/Windows and wait
  for Instruments notification registration before recording. Use the baseline
  ARM64 register mask for software-clock DWARF captures.
- Adapt canonical Windows paths for the ETL decoder and reject WPR traces that
  report dropped events instead of treating successful file saving as completeness.
- Decode Windows sampled CPU events through thread lifecycle ownership, accept
  native CRLF readiness lines, and preserve native WPR profile semantics while
  configuring bounded buffers and lossless stack caches. Require zero lost events.
- Keep failed attached LSP targets alive until collector cleanup runs, and
  classify already-buffered late responses as request timeouts.
- Configure WPR stack caches only on collector definitions; reference-level
  cache overrides rejected by the native WPR parser are no longer synthesized.
- Repair the pinned Windows ARM64 sampler's SampleProf/StackWalk join, preserving
  CPU deltas and separate kernel/user stacks. Bind its modified-source patch and
  installed binary identity in retained provenance.
- Retain the authoritative Callgrind policy verdict and actual workflow job/step
  outcomes so numerical regressions remain distinct from collection failures.
  Historical missing verdicts never imply a passing gate.
- Reject retained metric outcomes that contradict the producer's unchanged
  authoritative limits, even when hashes and every verdict flag are consistent.
- Initialize Instruments tracing/authorization services on a trusted system
  process before creating the cold LSP; retain the preparation trace separately.
  Actual capture readiness still requires its own notification within 60 seconds.
- Set and retain `MallocNanoZone=0` only for native macOS Allocations captures to
  remove allocating nano-zone enumeration during attach. These heap profiles
  reflect the scalable allocator, not default-allocator process memory.
  Clean latency/process-memory and DHAT collection remain unchanged.
- Keep large reports navigable by linking generated source inputs through the
  complete verified JSON artifact catalog and displaying identical scope
  warnings once; original files, identities and per-round evidence remain intact.
- Stop embedding resident sample arrays and manifest inventories repeatedly in
  HTML. Keep timelines, per-round request distributions and completion details;
  full samples and source inventories remain in verified JSON and original files.

- Move sccache compiler objects from GitHub storage to BuildFetch WebDAV with
  separate trusted-main writer and readonly consumer credentials. Keep GitHub
  caches for Cargo downloads and installed tools; fork PRs receive no remote
  cache credentials. Add a real write/restart/readonly-hit compiler probe.
- Preserve credential-redacted daemon diagnostics when the BuildFetch compiler
  probe encounters storage I/O failures.
- Correct BuildFetch routing to `/sccache/<projectId>` and omit the WebDAV key
  prefix, following the provider's correction to its generated setup instructions.
- Distinguish WebDAV directory-creation failures from authenticated object
  routing failures using reserved health-file HTTP diagnostics.
- Keep Cargo download and installed-tool cache identities independent of compiler
  cache routing, and revalidate pinned tool versions after compatible restores.
- Reuse compiler-cached libraries during cold quality-tool installation without
  sharing its target directory with project or performance builds.
- Move the trusted-main BuildFetch probe into an independent manual/push micro
  workflow to diagnose compiler caching without rerunning the native build matrix.
- Add snapshot-ownership regressions and Linux stdio FIFO barriers for watcher
  admission, missing/recreated dependencies, overlays, and late staging commits.
- Cache Cargo dependency inputs and pinned helper installations, including
  Cargo install metadata, with trusted-main writes and read-only consumers.
- Enable pinned sccache for quality/native builds and seed all six
  native platforms without publication. Measured Callgrind, calibration, and
  latency builds retain their independent fresh object/target construction.
- Install checksum-pinned official nextest 0.9.131 native archives instead of
  compiling the runner separately in every native and installer E2E job.
- Reuse same-run, exact-target nextest archives of the full native release suite
  for mandatory direct and installed-binary E2E on all six platforms, without
  duplicate debug test compilation or installer Cargo/sccache caches. Keep
  native Rust for runtime fixtures, every test gate, and strict flaky-test
  rejection; test archives remain internal and are never published.

## [0.5.0](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.4.0...v0.5.0) (2026-10-08)

### Better import completion

- Accept local and cached-package import suggestions without manually adding an
  alias. Keep explicit aliases and comments, replace the full path when the
  caret is in its middle, and generate valid collision-free ASCII aliases for
  nested, relative, and Unicode filenames.
- Keep `Base` imports unaliased, including Missing import quick fixes. Reuse the
  existing alias when completing another import of the same path.

### More reliable editing

- Show the actual alias, local, or resolved member name in Rename Symbol previews.
  Reject Rename Symbol on import paths with guidance to rename the file or folder
  in the project tree; never rewrite an alias or path segment instead.
  Alias rename keeps matching import path segments unchanged.

### Better completion

- Refresh import suggestions as you type, backspace, or continue after a path
  separator. Find indexed files by fuzzy file names or paths, with file-name
  exact and prefix matches ahead of fuzzy matches.
- Keep the popup's searchable file name separate from the full import path and
  insertion text. Document Zed's automatic-completion settings and inline
  prediction precedence.

- Accept a symbol suggestion from an already indexed module to insert its
  qualified name and missing import together. Suggestions show their source
  module, reuse existing aliases, and avoid duplicate imports and alias conflicts.
  Unsaved modules and imports are supported; browsing or cancelling leaves the
  document unchanged. In-scope suggestions retain priority over cross-module names.

### Editor instructions

- Document Zed's whole-document Organize Imports command and default shortcut,
  with before/after examples, safe sorting/deduplication rules, and the normal
  unchanged-buffer result. Cover unsaved LF/CRLF buffers and import-group
  boundaries with protocol regressions. Unused imports are not removed.

### Development

- Add default-off `dhat-heap` profiling for the actual LSP server, with
  `BEND2_LSP_DHAT_FILE` enabling profiling and selecting the heap JSON output
  (unset disables recording; empty selects the default path). Capture allocation totals,
  peak/end-live heap, and allocation stacks across server and Tokio runtime
  startup/shutdown; normal builds keep their existing allocator and release
  profile. This complements the historical Valgrind DHAT example measurements.

### Memory

- Halve private dense optional-index slot storage without narrowing public IDs.
  Count identifiers during cold scanning to avoid reserving reference rows for
  punctuation. A source-identical selfhost compiler/Base session measured 26.8%
  less peak live heap and 19.6% less allocation traffic; CPU/cache gates remain
  independent and unchanged.
- Build constructor completion descriptions with one exact-sized allocation.
  Add warm indexed import-candidate benchmarks for exact and fuzzy symbol queries.

## [0.4.0](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.3.0...v0.4.0) (2026-10-08)

### Better completion

- Find names by typing only part of them. Exact and prefix matches appear before
  fuzzy matches.
- Accept a suggestion in the middle of a name without leaving its old suffix
  behind, including around emoji and other Unicode text.
- Complete constructors inside nested case patterns. Known explicit types narrow
  the suggestions; unknown types keep the broader list.
- Complete imports for `Base`, local files, and already indexed cached packages.

### Navigation, rename, and imports

- Open `Base`, local, and cached package imports through clickable links.
- Rename import aliases without changing shadowed local names or member names.
- Rename files or folders and update imports in known workspace files while
  preserving open unsaved buffers.
- Add missing imports for available indexed symbols, reusing aliases and avoiding
  name conflicts or duplicates.
- Organize imports without losing comments, aliases, or existing line endings.

### Compiler integration and editor checks

- Show every error block the compiler actually emits with more accurate source
  ranges, including errors after Unicode text in unsaved imported files.
- Recover when a missing or incompatible compiler is installed or replaced.
  Check its CLI capabilities rather than reject it solely by version number.
- Exercise the actual server in Neovim as part of quality CI.

### Limits

Bend can still stop after its first independent error. Compiler-generated
semantic fixes and inferred types for arbitrary expressions need upstream APIs.
Whole-project discovery remains deferred; this release uses document snapshots
and workspace indexes rather than scanning every project file.

### Performance

The maintainer accepted the measured performance cost of these features in
[#32](https://github.com/IlyaGulya/bend2-lsp-rs/pull/32): constructor-prefix
completion uses 3.28% more instructions, and several instruction-cache metrics
exceed the existing thresholds. The separate latency checks passed. These
function-level benchmarks do not measure editor response time. Performance
thresholds and baseline rules are unchanged.



## [0.3.0](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.5...v0.3.0) (2026-10-07)


### Better autocomplete

- Function parameters and variables introduced in `case` branches now appear
  in suggestions.
- When writing a `case` pattern, autocomplete suggests constructors from your
  file, imported modules, and explicitly imported `Base`.
- Suggestions use your latest unsaved edits, even at the end of an unfinished
  file.
- Names from another scope no longer interfere with local suggestions.

### More reliable editing

- Improved handling of rapid edits and closing or reopening files, so older
  results cannot replace newer ones.
- Fixed cases where outdated diagnostics could remain visible.
- Improved recovery when reloading `Base` fails or returns no declarations.

### Under the hood

- Completed an internal cleanup to make the server easier to maintain.
- Kept the current workspace indexing model. Whole-project background indexing
  is **not included** in this release.

### Engineering details

For implementation history, regression coverage, measured performance
trade-offs, and preserved research, see
[#14](https://github.com/IlyaGulya/bend2-lsp-rs/pull/14),
[#23](https://github.com/IlyaGulya/bend2-lsp-rs/pull/23),
[#25](https://github.com/IlyaGulya/bend2-lsp-rs/pull/25),
[#26](https://github.com/IlyaGulya/bend2-lsp-rs/pull/26),
[#27](https://github.com/IlyaGulya/bend2-lsp-rs/pull/27),
[#28](https://github.com/IlyaGulya/bend2-lsp-rs/pull/28), and
[#30](https://github.com/IlyaGulya/bend2-lsp-rs/pull/30).
The [architecture decision](docs/adr-global-indexing.md) records why global
indexing and discovery remain deferred.

## [0.2.5](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.4...v0.2.5) (2026-10-03)


### Bug Fixes

* preserve compiler-owned Base document provenance ([#11](https://github.com/IlyaGulya/bend2-lsp-rs/issues/11)) ([ab60a28](https://github.com/IlyaGulya/bend2-lsp-rs/commit/ab60a289e775231738a20bab4fff0c89dafe1489))

- Track compiler-owned `Base` source in the workspace index so navigation does not
  trigger invalid standalone compiler checks. Retain its source and provenance
  across compiler configuration changes and editor close/reopen; preserve lexical
  diagnostics and normal checks for user-owned files named `Base.bend`.

## [0.2.4](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.3...v0.2.4) (2026-10-03)


### Features

* add generated cross-platform release installers ([#8](https://github.com/IlyaGulya/bend2-lsp-rs/issues/8)) ([be1cab9](https://github.com/IlyaGulya/bend2-lsp-rs/commit/be1cab9cfbdaa3393195ee8a09184160577373dd))

- Preserve direct binaries and immutable releases; verify archive SHA256 before extraction and exercise installed binaries on all six native platforms.

## [0.2.3](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.2...v0.2.3) (2026-10-02)


### Bug Fixes

* omit release versions from asset filenames ([#6](https://github.com/IlyaGulya/bend2-lsp-rs/issues/6)) ([20910ec](https://github.com/IlyaGulya/bend2-lsp-rs/commit/20910ecb625ef9f14caaf50d9ce26aae7622a12b))

- Retain version/SHA checks in release metadata; previously published assets remain unchanged.

## [0.2.2](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.1...v0.2.2) (2026-10-02)

### Bug Fixes

- Respect lexical parameter shadowing in declaration navigation and hover, including shadowed module qualifiers and scoped parameter annotations.
- Reject unsupported module-alias rename instead of renaming an imported member under a different cursor.
- Preserve body indentation after colonless declarations during newline formatting; handle comments, quoted delimiters, and incomplete literals safely.
- Retain watched closed-file updates across concurrent document commits and preserve newer open-buffer import edges until close.
- Wait for relevant imported-document revisions in cross-file queries without blocking unrelated documents during cold snapshot construction.
- Update revision readiness atomically so older completion or cancellation cannot overwrite a newer pending revision.
- Kill and reap active compiler-check and `bend base` children before shutdown or process exit completes; preserve configured compiler arguments, buffered responses at stdin EOF, and staging resources until child cleanup finishes.
* preserve lexical bindings, workspace revisions and compiler lifecycle ([#4](https://github.com/IlyaGulya/bend2-lsp-rs/issues/4)) ([e08cace](https://github.com/IlyaGulya/bend2-lsp-rs/commit/e08cace9088ff37ec0789bf9694b1a474093f368))

### CI

- Add report-only Callgrind calibration with independent A/A builds, layout and positive-regression controls, preserved raw evidence, and discovery-only proposals checked against independent validation; active gates remain unchanged.
- Preserve raw baseline/candidate Callgrind profiles and summaries after both successful and failed comparisons, without changing metrics, limits, baseline selection, or gate results.


## [0.2.1](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.0...v0.2.1) (2026-10-02)


### Bug Fixes

* resolve imported constructor definitions and track LSP latency ([57d9f76](https://github.com/IlyaGulya/bend2-lsp-rs/commit/57d9f76fe8ae47d84f098e67358e178027a2a69f))


### Performance Improvements

* iterate indexed reference slices directly ([a11ef5d](https://github.com/IlyaGulya/bend2-lsp-rs/commit/a11ef5dc633db0451fb48805b57feb215661fbc0))

## [0.2.0](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.1.1...v0.2.0) (2026-10-01)


### Features

* automate tested native nightly and versioned releases ([804ea24](https://github.com/IlyaGulya/bend2-lsp-rs/commit/804ea24b9b653c884942123f8497fb558ce5546c))


### Bug Fixes

* accept upstream v-prefixed pinned tool versions ([38b3637](https://github.com/IlyaGulya/bend2-lsp-rs/commit/38b36372de4937d3de40cddd70a24bf8048963cc))
* harden GitHub Actions and publish direct release binaries ([d9a535a](https://github.com/IlyaGulya/bend2-lsp-rs/commit/d9a535a02154838ad3f11aca658beb01c974cb45))
* preserve native release verification across macOS and Windows ([1f224ba](https://github.com/IlyaGulya/bend2-lsp-rs/commit/1f224ba6828b63fae17136f688ff9488bfb7f251))
* replace policy label gate with PR review and update Node actions ([c2e5fbc](https://github.com/IlyaGulya/bend2-lsp-rs/commit/c2e5fbc582821811f576c37e2c3f999b6f34aebe))
