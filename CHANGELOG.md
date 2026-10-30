# Changelog

## Unreleased

### Live lint diagnostics

- Check unsaved Bend buffers, their edited imports, and named entry/imported files
  not yet on disk, including paths below missing parent directories, with the
  optional standalone bend-lint provider. External tool and project rule execution
  still require explicit opt-in.
- Prepare the rule runner separately from ordinary checks. Typing does not restart
  a cold build, and short per-check deadlines do not interrupt preparation.
  Switching projects reuses their prepared runners, and editing one project no
  longer cancels checks in another.
- Target standalone bend-lint 0.2.1, Bun 1.4.4, and matching rules/API 0.2.1.0.
  It uses its own pinned compiler: Bend 2.0.39 on Unix and an upstream Windows fork.
- Show rule documentation links and explain partial checking, skipped rules, and
  configuration/setup failures through lint status and editor messages instead of
  inventing source ranges or document-wide error-count banners.
- Keep compiler, lexical, imported, and lint diagnostics merged. Reject obsolete
  results after edits, close, configuration changes, or source alias retargeting.
  Unrelated projects and unchanged indexed inputs retain their valid results.
- Automatically check already-open files when lint is enabled. Follow configurations
  created or removed after opening a file, including nested project configurations.
  Recheck watched dependencies and external rule helpers changed during preparation.
- Preserve independent buffers and versions for equivalent editor URI spellings.
  Show lint findings in every matching buffer; identify conflicting buffers before
  executing project rules, without blocking unrelated projects.
- Offer explicitly requested **dangerous** Code Actions for matching open buffer
  versions, including unsaved text. Validate UTF-16 ranges and overlapping edits,
  and refuse fixes for ambiguous physical-file aliases. Reject stale or unverifiable
  multi-file edits as a whole. Never apply automatic fixes, expose a safe fix-all,
  or replace the independent formatter.
- Keep lint highlights valid on CRLF files and after non-BMP characters without
  relaxing the exact source boundaries required for dangerous edits.
- Keep Run and Status Code Actions discoverable, with progress on supporting
  clients and actionable preparation/failure messages. Repeated identical failures
  do not repeatedly interrupt editing with popups.
- Bound snapshots, overlay input, and analyzer output; contain and reap subprocess
  trees on cancellation, timeout, disable, and shutdown. Preserve filesystem alias
  and missing-import guards without serializing lossy native paths.
- Exercise the published analyzer through the actual stdio LSP and retain native
  runtime/package identity evidence on the existing six-target CI matrix.
- Report the upstream 0.2.1 limitation for overlay-only files listed in config
  `entries` honestly; never write editor buffers to disk or drop configured entries
  to conceal that failure.
- Fence local custom-rule helpers as trusted saved code, including aliases and
  missing dependencies. Upstream 0.2.1 still omits bare/absolute helper imports
  from its compiled-rule cache key; use `./` or `../` in custom rules until
  [upstream issue #7](https://github.com/MattCozendey/bend-lint/issues/7) is fixed.

### Bug Fixes

- Keep `import Base` first when organizing imports, preventing repeated bend-lint
  import-order warnings after applying the action.
- Keep compiler errors visible in each matching open URI spelling, using that
  buffer's own document version rather than another spelling's revision.
- Keep lint checks working when entries reach the same project through directory
  symlinks, instead of skipping an entry because its project path is spelled differently.
- Check saved Linux entries with non-UTF-8 filenames through their UTF-8 aliases.
  Keep diagnostics and explicit versioned fixes on the alias without changing source bytes.
- Stop immediate preparation failures leaving lint stuck in preparation. Older attempts
  cannot replace the identity or outcome of a newer preparation.
- Remove outdated lint warnings and fixes immediately when watched inputs change,
  even while cleanup is pending. Keep any newer results and fixes.
- Keep edits in unrelated projects responsive while lint quick fixes wait for
  another project's running analyzer.
- Offer manual lint quick fixes when an editor omits optional request trigger
  metadata. Explicitly automatic requests still cannot receive dangerous fixes.

### Under the hood

- Coordinate lint work and cancellation per project, with one immutable input view
  per revision across entry checks. Keep exact source and document-version fences
  when disk inputs change during analysis.
- Separate lint lifecycle transitions, scheduling, read-set capture, saved-rule trust,
  source overlays and editor-view selection. Share captured input tracking without
  treating unsaved source text as executable rule code.
- Record an unpaired CI publication baseline with 32 entries and 128 diagnostic
  targets against optimized native executables. Validate exact current findings and
  retain source/binary/harness/fixture identity; local correctness mode emits no timing
  samples. Keep existing performance metrics, comparison limits and admission unchanged.
- Gather dependent documents directly into the result buffer, without per-file
  vectors or moving owned documents through composite iterator state.
- Keep representative snapshot lookup separate from editor-view metadata. Share
  immutable URI allocations across returned documents, staged updates, lint revision
  fences and compiler modules instead of copying URL payloads. Preserve each spelling's
  independent buffer, language, version and close/rename ownership. Updating the active
  buffer does not rebuild unchanged URI/language metadata or clone an unused URI.
  Intern owned URLs without a preliminary shared-key lookup, and create a shared
  URI key only when inserting a new spelling.
- Raise Tokio's minimum version and lock it to 1.53.3, clearing
  [RUSTSEC-2026-0386](https://rustsec.org/advisories/RUSTSEC-2026-0386)
  without adding an audit exception.
- Synchronize manual lint E2Es with completed analyzer runs, rather than buffered
  automatic publications, before checking cross-entry fixes and save rechecks.
- Capture canned analyzer reports before signalling test readiness, and prepare the
  next report before reopening a document. Keep close/reopen epoch checks free of
  concurrent fixture-file rewrites.
- Publish fixture socket addresses atomically, so cancellation checks never read
  an empty or partially written address. Keep flaky-test rejection unchanged.
- Compile the Windows console-control fixture directly with the system .NET
  Framework compiler, without PowerShell/Add-Type startup. Keep bounded setup,
  collector readiness and CTRL_BREAK finalization deadlines unchanged.
- Validate source-built Windows compilers at their final physical path instead of
  moving the directory after execution. Retain failed artifacts for diagnostics;
  publish the compiler path only after every integrity, native-runtime and CLI probe.
- Keep view epochs on existing file rows instead of a separate hash table, and
  reuse resolved file identities when closing documents. Compare ASCII symbol
  names without allocating a lowercase copy per name; preserve Unicode query
  normalization, result order and independent editor views.
- Reduce autocomplete lookup and matching overhead using indexed declaration
  name ranges and query-wide ASCII/non-ASCII specialization. Keep one matching
  pass, ordinary result-buffer growth and existing snapshot metadata. Preserve
  suggestion contents, order, fuzzy matching and keyword suppression.
- Fail ordinary performance admission on regressions without automatically
  requesting approval. Protected exact-run owner review is available only after
  an explicitly requested manual comparison; numeric limits remain unchanged.


## [0.6.2](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.6.1...v0.6.2) (2026-10-15)

### Bug Fixes

- Support enhanced multi-error diagnostics with verified Bend 2.0.37 builds.
  Preserve independent errors, original-cause links, and precise source ranges.
- Keep enhanced compiler checks working when project and temporary paths contain
  spaces or non-ASCII characters.
- Resolve local imports during enhanced Windows checks, including transitive
  imports, and keep related errors linked to the original declarations.

### Under the hood

- Gate performance comparisons with 105 fresh source pairs after full prospective
  A/A and positive-control qualification. Keep numeric limits unchanged; block
  inconclusive results instead of treating uncertainty as a pass or regression.
- Bind control evidence to both benchmark executables and all 54 workloads,
  including reference ordering. Preserve the original benchmark harnesses in
  ordinary source comparisons and retain raw failures without retries for luck.
- Recompute admission verdicts from retained Callgrind counters and raw output.
  Bind results to the exact source, workflow, run and attempt; reject stale or
  contradictory evidence. Show exact uncertainty bounds in HTML and Markdown
  reports while preserving historical scalar evidence as historical.
- Require genuine compiler diagnostics in all six native and installed-executable
  E2E matrices. Pin official Unix archives and build the Windows compiler from
  exact upstream source with a pinned native Bun runtime and a resource-locator
  adjustment only. Retain macOS ARM64 Bend 2.0.36 compatibility coverage.
- Use native filesystem paths when loading compiler sources; keep Bend import
  namespaces slash-separated. Validate these loader adaptations against exact
  embedded-source anchors before applying any recovery patches.
- Avoid repeated temporary URI string allocations when ordering cross-file
  references. Preserve stable result ordering and full-range deduplication.
- Measure reference ordering in an isolated benchmark binary and include its
  collected workloads in the same strict, source-bound Callgrind admission.
- Prepare semantic-token counts and compact, ordered argument/parameter links
  during cold indexing instead of regrouping and sorting hints in warm queries.
  Build ordered hints directly from calls, inserting nested argument interleavings
  instead of running a second generic sort.
  Reuse grouping counts as write cursors and skip hint construction when indexing
  proves there are no eligible arguments.
  Keep scoped completion name deduplication compact and reuse indexed import identities.
- Avoid rereading declaration-name slices during autocomplete and pre-size
  workspace document and dependent-traversal buffers from existing index bounds.
  Preserve local shadowing, snapshot freshness and cyclic/transitive traversal.
  Avoid a visited set and traversal-stack allocation for terminal reverse
  dependencies, preserving deduplication and cycles for transitive frontiers.
- Look up declaration names through existing one-word dense index slots instead
  of hashing indexed name IDs. Preserve first-declaration precedence, qualified
  names and local shadowing. Read reference target IDs without loading declaration
  metadata. Prepare both ASCII case alternatives per query byte instead of
  repeatedly folding candidate-label bytes.
- Keep unscoped autocomplete on the declaration-only query path, without building
  or sorting an empty local-binding set. Project module completions directly into
  their result vector and share the existing keyword projection.
  Classify the query's ASCII prefix once across declaration and keyword suggestions.
- Reject empty instruction collection against a nonzero benchmark baseline
  instead of treating missing measurements as a performance improvement.
- Pin the Linux Callgrind profiler to checksum-verified Valgrind 3.27.1 so
  supported Rust executable layouts retain genuine function-level collection.
  Collect baseline and candidate with the same tool and unchanged strict limits.

## [0.6.1](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.6.0...v0.6.1) (2026-10-13)

### Bug Fixes

- Show native compiler dependency errors on the affected import path, with links
  to the original declarations, including through transitive imports. Keep
  independent source errors visible instead of showing a document-wide count of
  blocked declarations. Checks without a located cause remain explicit failures.
- Stop declaration error underlines at the last syntax token, excluding following
  comments and blank lines, using the native parser's existing trivia provenance.
- Run mandatory macOS ARM64 stdio E2Es with the genuine checksum-pinned Bend
  2.0.36 distribution in quality CI and the native release suite. Cover source and
  import ranges, original-cause links, unsaved diagnostic cleanup, and the
  ownership-based distinction between compiler `Base` and ordinary user modules.


## [0.6.0](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.5.0...v0.6.0) (2026-10-13)

### Features

- Report multiple independent compiler errors, including imported-source errors,
  with verified native Bend 2.0.32–2.0.36 on Apple Silicon macOS. Use the installed
  compiler's parser/checker and embedded runtime, with exact identity validation.
  Other platforms and versions retain textual CLI diagnostics.
- Preserve independent errors when a declaration or imported source is damaged;
  explicitly report incomplete checks and backend failures. Bind structured
  locations to immutable staged source digests and UTF-16 ranges, preserve unsaved
  dependency overlays, and retain staging until compiler children are reaped.
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
- Build workspace-symbol results directly from immutable indexed declarations.
  Empty queries avoid per-name lowercase allocations and intermediate per-file
  result arrays; filtered queries normalize once across the workspace while
  preserving Unicode case matching.
- Encode typed successful JSON-RPC responses without building an intermediate
  JSON object tree, including full workspace-symbol responses. Maintain a
  Tokio-only `tower-lsp` 0.20.0 fork pinned to upstream
  `7b580128faa39b907db8f40bc53924181887acde`, with its original licenses,
  behavioral tests and documentation examples. Preserve incoming message
  decoding, byte-accurate framing, cancellation and shutdown behavior; this
  is not a measured claim about editor latency.

### CI

- Retain one bounded live stack sample of an owned macOS Instruments recorder
  still finalizing after 280 seconds. Keep the 300-second finalization deadline
  and failed-capture verdict; this adds diagnostic evidence, not a timeout fix.
- Use platform-native absolute paths in compiler-backend protocol fixtures,
  including Windows drive roots. Preserve rejection of dot, parent, and
  repeated-separator aliases. Run the six-platform native release and installer
  checks for backend fixture and release manifest changes before merging.
- Add workspace-symbol Callgrind cases for empty and mixed-case queries over the
  existing indexed 100-file workspace. Preserve owned response names, kinds,
  ranges and file identities; retain the existing metrics and regression gates.
- Separate validated ordinary Callgrind measurement from `performance admission`.
  Preserve raw producer exits, source-bound verdicts, profiles, and historical
  red results. Require a protected, exact-run owner review for valid regressions;
  verify the sole-owner reviewer rule and disabled administrator bypass before
  scheduling it. Require a fresh matching owner/environment approval-history
  entry before admitting, rather than infer approval from job success or reuse
  an earlier run attempt's approval. Collector/malformed-evidence errors stay
  red, and prior exceptions never authorize later candidates.
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
- Prepare static imported-source diagnostic fixtures before initializing the
  server, so background discovery cannot cache an unnotified partially written
  fixture. Preserve exact ambiguity/range assertions and fail-if-flaky policy.
- Exercise three repeated 10,000-file workspace lifecycles in the quality
  workflow, checking exact LSP results across unsaved edits, watched deletion
  and recreation, close, and superseded root removal/readdition. Retain failure
  evidence and report-only RSS checkpoints; separately verify indexed snapshot
  retirement with weak handles without treating RSS as allocation-liveness proof.
- Deliver sampler stop signals to the owned recorder on macOS/Windows and wait
  for Instruments notification registration before recording. Use the baseline
  ARM64 register mask for software-clock DWARF captures.
- Adapt canonical Windows paths for the ETL decoder and reject WPR traces that
  report dropped events instead of treating successful file saving as completeness.
- Decode Windows sampled CPU events through thread lifecycle ownership, accept
  native CRLF readiness lines, and preserve native WPR profile semantics while
  configuring bounded buffers and lossless stack caches. Require zero lost events.
- Increase only Windows native Heap-profile pools after retained workload-phase
  rejection in both owned loggers: 256 event and 1024 heap buffers of 1 MiB.
  CPU/system pools, full native providers/stacks, caches and deadlines stay
  unchanged; a fresh zero-loss capture is required before acceptance.
- Keep failed attached LSP targets alive until collector cleanup runs, and
  classify already-buffered late responses as request timeouts.
- Preserve live macOS target stacks after DHAT and native heap request timeouts,
  without retries or longer request deadlines. Retain WPR's pre-stop collector
  counters to distinguish rejected events from lost log buffers; stop the owned
  recorder before reporting counter-command failures.
- Resolve heap profiling sessions from `--heap`, not the native CPU default, so
  macOS DHAT request timeouts retain the promised live target stack. Use the same
  profile kind in preflight and retained provenance without changing deadlines.
- Launch macOS native allocation captures before server runtime startup, using
  a separate default-off profiling binary and real FIFO protocol endpoints.
  Preserve target exit status independently of Instruments, reject substituted
  endpoints, and validate the exact launched process and allocation payload.
  Ordinary server, CPU, timing and DHAT builds remain unchanged.
  Keep malformed-trace rejection independent of checkout line endings.
- Configure WPR stack caches only on collector definitions; reference-level
  cache overrides rejected by the native WPR parser are no longer synthesized.
- Repair the pinned Windows ARM64 sampler's SampleProf/StackWalk join, preserving
  CPU deltas and separate kernel/user stacks. Bind its modified-source patch and
  installed binary identity in retained provenance.
- Preserve LF patch bytes on Windows checkouts so the pinned ARM64 sampler
  bootstrap verifies the same SHA256 as the committed source.
- Replace whole-recording `tracerpt` XML export with a pinned Microsoft TraceEvent
  streaming decoder on Windows x64/ARM64. Keep full-EOF processing, real loss
  counters, target-process allocation/sample validation, raw ETL, and the
  existing timeout. Bind SDK, runtime, package lock, source and executable
  identity in native tool provenance.
- Disable persistent .NET build servers during the hosted ETL reader bootstrap
  so compiler processes cannot keep temporary SDK DLLs locked during cleanup.
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

### Commit references


**Features**

* unified hosted portable performance measurements and profiling ([447db8e](https://github.com/IlyaGulya/bend2-lsp-rs/commit/447db8e8829daf943987bef44186fff90415c269))
* unify hosted portable performance measurements and profiling ([a2a8404](https://github.com/IlyaGulya/bend2-lsp-rs/commit/a2a8404bb6702eb5b8b77254bc793d7aa6438fae))


**Bug fixes**

* bind DHAT timeout diagnostics to the actual heap profile kind ([0d10bd7](https://github.com/IlyaGulya/bend2-lsp-rs/commit/0d10bd7d24f8d80d4b5e083664108d1e919924de))
* finalize owned native recorders and preserve report evidence ([775a0f6](https://github.com/IlyaGulya/bend2-lsp-rs/commit/775a0f645db5e0d641ba58f13b166d84b1649fdd))
* keep temporary ETL SDK free of build-server leases ([91c8d76](https://github.com/IlyaGulya/bend2-lsp-rs/commit/91c8d76c163bc5a3b5231151d66d3691465d7d66))
* launch native macOS heap capture before runtime startup ([b62e4ef](https://github.com/IlyaGulya/bend2-lsp-rs/commit/b62e4efb499e6bb6c16e56885dce549d53b55c45))
* prepare native tracing services and retain allocator capture scope ([2698113](https://github.com/IlyaGulya/bend2-lsp-rs/commit/26981131227cccc3d5e1766a69691fc0587d9fa9))
* preserve native sampler CPU events and canonical gate verdicts ([28e659a](https://github.com/IlyaGulya/bend2-lsp-rs/commit/28e659a6b89bd1afdb969af230dfe854b795d901))
* preserve native trace evidence and collector ownership ([0eb0fd2](https://github.com/IlyaGulya/bend2-lsp-rs/commit/0eb0fd26e11a5ec827a7feb13d18e7bc38c28e65))
* reject contradictory gate evidence and bound report payloads ([6831d0a](https://github.com/IlyaGulya/bend2-lsp-rs/commit/6831d0af6f89ade0e3ba70a2282702a346dfcd3d))
* release orphan workspace snapshots and filter watcher admission ([#39](https://github.com/IlyaGulya/bend2-lsp-rs/issues/39)) ([c6b4307](https://github.com/IlyaGulya/bend2-lsp-rs/commit/c6b4307eb0981b44a85ea781562869c2e4294f75))
* reserve bounded capacity for both rejecting Windows heap loggers ([f5dcf87](https://github.com/IlyaGulya/bend2-lsp-rs/commit/f5dcf8747c82a72e19b3e1063453c882cb19d502))
* retain raw discovery evidence without duplicating report payload ([2275927](https://github.com/IlyaGulya/bend2-lsp-rs/commit/2275927aef91fe1814deb4d11d17890e2227e90b))
* stream Windows ETL decoding with pinned TraceEvent ([77105cb](https://github.com/IlyaGulya/bend2-lsp-rs/commit/77105cbc9338653bbcb9b584631038ed47372fa6))
* verify actual native profiler protocols and exports ([28c16b0](https://github.com/IlyaGulya/bend2-lsp-rs/commit/28c16b032f39fac9db433a544d73ff1a13e0bd81))

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
