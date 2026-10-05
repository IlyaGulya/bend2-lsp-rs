# Changelog

## Unreleased

### Features

- Prepare grouped workspace semantic contributions during cold staging; use
  revision-safe symbol identities and indexed occurrence/call buckets for warm
  cross-file queries. Keep stable file IDs with deletion tombstones.

### Bug Fixes

- Isolate close/reopen revision epochs so stale tickets, queued closes, and old
  diagnostics cannot overwrite reopened buffers with restarted version numbers.
- Clear retired imported diagnostics before a queued close can be superseded by
  a reopened buffer whose holes skip compiler diagnostics.
- Validate prepared-close disk snapshot identity before restoring imports.
- Detach and cancel diagnostics tasks without awaiting under the task-registry
  lock; reject new work after shutdown and drain owned compiler children.
- Treat state poisoning and unexpected worker failure as fatal invariant errors
  rather than silently returning missing features.
- Detach active indexed Base resolution when compiler configuration resets;
  failed or empty reloads cannot retain old call targets, while previously
  published generated source remains readable and successful reloads rebind.

### Architecture

- Give workspace, diagnostics, compiler, and registration services explicit
  ownership and validated commit boundaries. Extract the revision state machine
  and split server requests and syntax implementation without changing the
  immutable snapshot or compact index representation.
- Keep the current LSP framework; document the framework boundary evaluation.
  Extract sequential text editing without introducing a Rope or incremental
  parser.
- Reuse local syntax reference/call spans and canonical external contribution
  groups instead of duplicating workspace range maps. Reduce cold name lookups
  and scratch allocations while retaining immutable snapshot query indexes.
- Store imported reference-row ordinals and flat per-file caller-group columns;
  read ranges and reference kinds directly from immutable snapshots instead of
  copies. Keep bound keys with target spans instead of retaining cold templates
  and a separate key allocation. Share column storage across targets without
  rebuilding importers when dependency declarations change.
  Visit indexed reference rows during cold preparation and avoid a scratch
  target-map allocation for single-target files.
  Materialize external references from exact-length ordinal slices; declarations
  stay in local syntax spans, so external rows need no declaration filtering.
  Add a separate interleaved multi-target cold benchmark; existing workloads,
  destruction scope, metrics, and limits are unchanged.
- Cache local occurrence and function-call totals during syntax-index construction.
  Keep workspace contributions only for external relations; local-only feature
  queries continue to use their immutable snapshots. Cold snapshot benchmarks
  include this metadata work separately from prebuilt-snapshot semantic workloads.
- Separate allocation-free external reference group lookup from occurrence
  materialization using one shared indexed traversal and current-epoch validation.
  Add lookup-only absent-bucket, single-source, and three-source benchmarks at
  100, 1,000, and 10,000 files; setup interns absent-target names through an
  independent relation and validates exact sources outside measurement.
- Build a compact unresolved-qualified reference ordinal column during cold
  syntax construction. Semantic staging visits only these candidates and no
  longer rereads local qualifier resolution; warm queries and dynamic target
  epochs retain their existing immutable snapshot and workspace contracts.
- Reserve semantic-token results from a cold-built exact count and avoid
  unnecessary trailing-whitespace scanning when checking blank folding lines.

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
